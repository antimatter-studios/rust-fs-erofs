//! C ABI happy paths: mount, volume info, stat, umount.
//!
//! A staticlib doesn't re-export unmangled C symbols to integration
//! tests, so -- as in the sibling fs-ext4 suite -- we call the public
//! items in `fs_erofs::capi` directly instead of declaring
//! `extern "C" { fs_erofs_mount ... }`. That drives the exact code
//! behind every exported symbol; the linkage itself is proven by
//! downstream consumers that link `libfs_erofs.a` against
//! `include/fs_erofs.h`.

mod common;

use common::errno::*;
use common::{
    big_body, capi_fixture_image, capi_last_error, cchar_field_to_bytes, dir, file, temp_image,
    BIG_LEN, HELLO, LINK_TARGET, LONG_LINK_TARGET_LEN, OWNED_GID, OWNED_MTIME, OWNED_UID,
};
use fs_erofs::capi::*;
use fs_erofs::mkfs;
use std::ffi::CString;

/// ABI file-type byte values, from `fs_erofs_file_type_t` in the header.
const FT_UNKNOWN: u32 = 0;
const FT_REG: u32 = 1;
const FT_DIR: u32 = 2;
const FT_CHR: u32 = 3;
const FT_BLK: u32 = 4;
const FT_FIFO: u32 = 5;
const FT_SOCK: u32 = 6;
const FT_LNK: u32 = 7;

/// Mount the shared fixture from a real file, run `body`, then umount.
/// The `TempImage` is held for the whole closure so the backing file
/// outlives the mount.
fn with_fixture(body: impl FnOnce(*mut fs_erofs_fs_t)) {
    let img = temp_image(&capi_fixture_image());
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "mount failed: {}", capi_last_error());
    body(fs);
    unsafe { fs_erofs_umount(fs) };
}

fn stat_path(fs: *mut fs_erofs_fs_t, path: &str) -> Option<fs_erofs_attr_t> {
    let c = CString::new(path).unwrap();
    let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_erofs_stat(fs, c.as_ptr(), &mut attr) };
    if rc == 0 {
        Some(attr)
    } else {
        None
    }
}

// ---- mount / umount ----------------------------------------------------

#[test]
fn mount_from_path_then_umount() {
    let img = temp_image(&capi_fixture_image());
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "mount failed: {}", capi_last_error());
    // A successful call must leave the thread-local error slots clean,
    // otherwise a caller that only checks `last_errno` after a later
    // no-error call would see a stale failure.
    assert_eq!(fs_erofs_last_errno(), 0);
    assert_eq!(capi_last_error(), "");
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn mount_missing_path_reports_enoent() {
    // "the device isn't there" and "the device is unreadable/damaged"
    // are different problems with different caller responses (offer to
    // pick another disk vs. offer to repair). The only channel the ABI
    // has for that distinction is `last_errno`, so a missing path must
    // report ENOENT, not the catch-all EIO.
    let missing = CString::new("/nonexistent-dir-xyz/erofs-not-here.img").unwrap();
    let fs = unsafe { fs_erofs_mount(missing.as_ptr()) };
    assert!(fs.is_null(), "mount of a missing path must fail");
    assert_eq!(
        fs_erofs_last_errno(),
        ENOENT,
        "missing device path must be ENOENT, got: {}",
        capi_last_error()
    );
    assert!(
        capi_last_error().contains("open"),
        "message should name the failed operation, got: {}",
        capi_last_error()
    );
}

#[test]
fn mount_path_that_is_a_directory_reports_the_os_errno() {
    // Opening a directory as a block device fails with EISDIR on macOS
    // and succeeds-then-fails-to-read on some other platforms. Either
    // way the driver must report a *specific* failure, never succeed.
    let dir_guard = tempfile::tempdir().unwrap();
    let p = CString::new(dir_guard.path().to_str().unwrap()).unwrap();
    let fs = unsafe { fs_erofs_mount(p.as_ptr()) };
    assert!(fs.is_null(), "mounting a directory must fail");
    assert_ne!(fs_erofs_last_errno(), 0, "a failure must set an errno");
    assert!(!capi_last_error().is_empty());
}

#[test]
fn mount_non_erofs_file_reports_einval() {
    // Bad magic is a *format* rejection, distinct from an I/O failure:
    // the bytes were read fine, they just aren't EROFS. EINVAL, not EIO.
    let img = temp_image(&vec![0u8; 8192]);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(fs.is_null(), "an all-zero image is not EROFS");
    assert_eq!(fs_erofs_last_errno(), EINVAL);
    assert!(
        capi_last_error().contains("not an EROFS image"),
        "got: {}",
        capi_last_error()
    );
}

#[test]
fn mount_corrupt_blkszbits_reports_einval() {
    // blkszbits outside 9..=16 is a malformed superblock -- again a
    // format problem, so EINVAL rather than EIO.
    let mut img = capi_fixture_image();
    img[1024 + 0x0C] = 99;
    let t = temp_image(&img);
    let fs = unsafe { fs_erofs_mount(t.path.as_ptr()) };
    assert!(fs.is_null());
    assert_eq!(fs_erofs_last_errno(), EINVAL);
    assert!(
        capi_last_error().contains("malformed superblock"),
        "got: {}",
        capi_last_error()
    );
}

#[test]
fn mount_truncated_image_reports_eio() {
    // The file exists and opens, but the superblock at byte 1024 can't
    // be read. That is a genuine I/O failure -> EIO, and it is what
    // separates "damaged volume" from the ENOENT / EINVAL cases above.
    let img = temp_image(&vec![0u8; 512]);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(fs.is_null(), "a 512-byte file has no superblock");
    assert_eq!(fs_erofs_last_errno(), EIO);
    assert!(
        capi_last_error().contains("block device"),
        "got: {}",
        capi_last_error()
    );
}

#[test]
fn mount_empty_or_null_path_is_einval() {
    let empty = CString::new("").unwrap();
    let fs = unsafe { fs_erofs_mount(empty.as_ptr()) };
    assert!(fs.is_null());
    assert_eq!(fs_erofs_last_errno(), EINVAL);

    let fs = unsafe { fs_erofs_mount(std::ptr::null()) };
    assert!(fs.is_null(), "NULL device_path must fail, not deref");
    assert_eq!(fs_erofs_last_errno(), EINVAL);
    assert!(
        capi_last_error().contains("device_path"),
        "got: {}",
        capi_last_error()
    );
}

#[test]
fn umount_null_is_a_safe_noop() {
    // The documented free-once contract means callers NULL their handle
    // after umount; umount(NULL) has to tolerate that rather than
    // double-freeing.
    unsafe { fs_erofs_umount(std::ptr::null_mut()) };
    let img = temp_image(&capi_fixture_image());
    let mut fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null());
    unsafe { fs_erofs_umount(fs) };
    fs = std::ptr::null_mut();
    unsafe { fs_erofs_umount(fs) };
}

// ---- volume info -------------------------------------------------------

#[test]
fn volume_info_mirrors_the_superblock() {
    let bytes = capi_fixture_image();
    // Cross-check against the Rust reader rather than against constants
    // baked into the test: the ABI's job is to surface the superblock
    // faithfully, not to invent values.
    let expect = common::open_image(bytes.clone());
    let sb = expect.superblock();

    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());

    let mut info: fs_erofs_volume_info_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_erofs_get_volume_info(fs, &mut info) };
    assert_eq!(rc, 0, "get_volume_info failed: {}", capi_last_error());

    assert_eq!(info.block_size, 4096, "fixture is built with blkszbits=12");
    assert_eq!(info.block_size as u64, sb.block_size());
    assert_eq!(info.total_blocks, sb.blocks);
    assert!(info.total_blocks > 0);
    assert_eq!(info.inode_count, sb.inos);
    assert!(info.inode_count > 0);
    assert_eq!(info.build_time, sb.build_time);
    assert_eq!(info.uuid, sb.uuid);
    assert_eq!(info.feature_compat, sb.feature_compat);
    assert_eq!(info.feature_incompat, sb.feature_incompat);

    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn volume_info_zero_fills_before_writing() {
    // The struct is caller-allocated, so any field the driver doesn't
    // set must be zeroed rather than left holding the caller's garbage
    // -- otherwise `volume_name` could come back unterminated.
    let img = temp_image(&capi_fixture_image());
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null());

    let mut info: fs_erofs_volume_info_t = unsafe { std::mem::zeroed() };
    unsafe { std::ptr::write_bytes(&mut info as *mut fs_erofs_volume_info_t, 0xAA, 1) };
    let rc = unsafe { fs_erofs_get_volume_info(fs, &mut info) };
    assert_eq!(rc, 0);

    // The fixture does carry a label, so the bytes are not all zero. What
    // must be true is that none of the caller's 0xAA fill survives and the
    // name is NUL-terminated -- an unterminated name would run a C caller
    // off the end of the field.
    assert!(
        !info.volume_name.iter().any(|&c| c as u8 == 0xAA),
        "volume_name kept the caller's fill: {:?}",
        info.volume_name
    );
    assert!(
        info.volume_name.contains(&0),
        "volume_name is not NUL-terminated: {:?}",
        info.volume_name
    );
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn volume_info_copies_a_label_and_nul_terminates_it() {
    // `volume_name` lives at superblock offset 0x40, i.e. file offset
    // 1024 + 0x40. Stamp a label in and check it comes back as a proper
    // C string.
    let mut bytes = capi_fixture_image();
    let label = b"EROFSVOL";
    bytes[1088..1088 + label.len()].copy_from_slice(label);
    let img = temp_image(&bytes);

    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());
    let mut info: fs_erofs_volume_info_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { fs_erofs_get_volume_info(fs, &mut info) }, 0);
    assert_eq!(cchar_field_to_bytes(&info.volume_name), label);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn volume_info_truncates_a_full_width_label_to_keep_the_terminator() {
    // The on-disk field is 16 bytes with no room reserved for a NUL, but
    // the ABI declares `char volume_name[16]` as NUL-terminated. A label
    // that fills all 16 on-disk bytes must therefore come back as 15
    // characters plus a terminator -- never 16 unterminated bytes.
    let mut bytes = capi_fixture_image();
    let label = b"ABCDEFGHIJKLMNOP"; // exactly 16 bytes, no NUL
    bytes[1088..1088 + 16].copy_from_slice(label);
    let img = temp_image(&bytes);

    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());
    let mut info: fs_erofs_volume_info_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { fs_erofs_get_volume_info(fs, &mut info) }, 0);
    assert_eq!(cchar_field_to_bytes(&info.volume_name), &label[..15]);
    assert_eq!(info.volume_name[15], 0);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn volume_info_rejects_null_fs_or_info() {
    let mut info: fs_erofs_volume_info_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { fs_erofs_get_volume_info(std::ptr::null_mut(), &mut info) };
    assert_eq!(rc, -1);
    assert_eq!(fs_erofs_last_errno(), EINVAL);

    with_fixture(|fs| {
        let rc = unsafe { fs_erofs_get_volume_info(fs, std::ptr::null_mut()) };
        assert_eq!(rc, -1, "NULL info must fail, not be written through");
        assert_eq!(fs_erofs_last_errno(), EINVAL);
    });
}

// ---- stat --------------------------------------------------------------

#[test]
fn stat_root_is_a_directory() {
    with_fixture(|fs| {
        let attr = stat_path(fs, "/").expect(" stat / must succeed");
        assert_eq!(attr.file_type, FT_DIR);
        // `mode` carries permission bits only -- the header tells callers
        // to reconstruct st_mode by OR-ing in the type implied by
        // `file_type`, so the S_IFDIR bits must NOT be present here.
        assert_eq!(attr.mode, 0o755);
        assert_eq!(attr.mode & 0o170000, 0, "type bits leaked into mode");
        assert!(attr.link_count >= 2, "a directory links itself via '.'");
        assert!(attr.size > 0, "root holds dirents");
    });
}

#[test]
fn stat_regular_file_reports_size_and_type() {
    with_fixture(|fs| {
        let attr = stat_path(fs, "/hello.txt").expect("stat /hello.txt");
        assert_eq!(attr.file_type, FT_REG);
        assert_eq!(attr.size, HELLO.len() as u64);
        assert_eq!(attr.mode, 0o644);
        assert_eq!(attr.link_count, 1);
        assert!(attr.inode > 0, "NID of a non-root inode is never 0");

        let big = stat_path(fs, "/big.bin").expect("stat /big.bin");
        assert_eq!(big.size, BIG_LEN as u64);

        let empty = stat_path(fs, "/empty.bin").expect("stat /empty.bin");
        assert_eq!(empty.size, 0);
        assert_eq!(empty.file_type, FT_REG);
    });
}

#[test]
fn stat_reports_uid_gid_and_mtime_from_the_extended_inode() {
    with_fixture(|fs| {
        let attr = stat_path(fs, "/owned.txt").expect("stat /owned.txt");
        assert_eq!(attr.uid, OWNED_UID);
        assert_eq!(attr.gid, OWNED_GID);
        // The ABI narrows EROFS's 64-bit mtime to uint32_t; the fixture
        // timestamp is well inside that range, so it must survive intact.
        assert_eq!(attr.mtime, OWNED_MTIME as u32);
    });
}

#[test]
fn stat_does_not_follow_symlinks() {
    with_fixture(|fs| {
        // The header states symlinks are NOT followed. If stat resolved
        // them, /link would report FT_REG and hello.txt's size.
        let attr = stat_path(fs, "/link").expect("stat /link");
        assert_eq!(attr.file_type, FT_LNK);
        assert_eq!(attr.size, LINK_TARGET.len() as u64);
        assert_ne!(attr.size, HELLO.len() as u64, "symlink was followed");

        let long = stat_path(fs, "/longlink").expect("stat /longlink");
        assert_eq!(long.file_type, FT_LNK);
        assert_eq!(long.size, LONG_LINK_TARGET_LEN as u64);
    });
}

#[test]
fn stat_maps_every_special_file_type() {
    with_fixture(|fs| {
        for (path, want, mode) in [
            ("/chr", FT_CHR, 0o600),
            ("/blk", FT_BLK, 0o660),
            ("/fifo", FT_FIFO, 0o644),
            ("/sock", FT_SOCK, 0o755),
        ] {
            let attr = stat_path(fs, path).unwrap_or_else(|| panic!("stat {path}"));
            assert_eq!(attr.file_type, want, "{path} file_type");
            assert_eq!(attr.mode, mode, "{path} mode");
        }
    });
}

#[test]
fn stat_reports_ft_unknown_for_an_inode_with_an_unrecognised_type() {
    // A damaged inode whose S_IFMT bits are none of the seven POSIX
    // types must degrade to FS_EROFS_FT_UNKNOWN. Reporting some other
    // type would make a consumer treat garbage as a file or directory.
    // 0o030000 is a deliberately invalid S_IFMT value.
    let mut bytes = capi_fixture_image();
    {
        let fs = common::open_image(bytes.clone());
        let inode = fs.lookup_path("/hello.txt").unwrap();
        let off = fs_erofs::inode::Inode::iloc(fs.superblock(), inode.nid) as usize;
        // `mode` is at inode offset 0x04 in both the compact and the
        // extended on-disk shapes.
        bytes[off + 4..off + 6].copy_from_slice(&0o030000u16.to_le_bytes());
    }
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());
    let attr = stat_path(fs, "/hello.txt").expect("stat of a typeless inode still succeeds");
    assert_eq!(attr.file_type, FT_UNKNOWN);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn stat_nested_paths_resolve() {
    with_fixture(|fs| {
        assert_eq!(stat_path(fs, "/sub").expect("stat /sub").file_type, FT_DIR);
        assert_eq!(
            stat_path(fs, "/sub/deeper/leaf.txt")
                .expect("stat leaf")
                .size,
            b"leaf\n".len() as u64
        );
        // Redundant and trailing separators are dropped by the path
        // splitter, so these name the same inode.
        let a = stat_path(fs, "/sub/nested.txt").unwrap();
        let b = stat_path(fs, "//sub//nested.txt/").unwrap();
        assert_eq!(a.inode, b.inode);
    });
}

#[test]
fn stat_empty_path_resolves_to_the_root() {
    // Documented consequence of the C ABI's path splitting: an empty
    // path has no components, so resolution stops at the root. Pinned
    // here so it can't change silently.
    with_fixture(|fs| {
        let root = stat_path(fs, "/").unwrap();
        let empty = stat_path(fs, "").expect("empty path resolves to root");
        assert_eq!(empty.inode, root.inode);
        assert_eq!(empty.file_type, FT_DIR);
    });
}

#[test]
fn stat_missing_path_reports_enoent() {
    with_fixture(|fs| {
        let c = CString::new("/no-such-file").unwrap();
        let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_erofs_stat(fs, c.as_ptr(), &mut attr) };
        assert_eq!(rc, -1);
        assert_eq!(fs_erofs_last_errno(), ENOENT);
        assert!(
            capi_last_error().contains("/no-such-file"),
            "message should name the path, got: {}",
            capi_last_error()
        );
    });
}

#[test]
fn stat_through_a_non_directory_reports_enotdir() {
    // "/hello.txt/x" asks to descend into a regular file. POSIX calls
    // that ENOTDIR, and the distinction matters: ENOENT would send a
    // caller looking for a missing name that never existed.
    with_fixture(|fs| {
        let c = CString::new("/hello.txt/x").unwrap();
        let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_erofs_stat(fs, c.as_ptr(), &mut attr) };
        assert_eq!(rc, -1);
        assert_eq!(fs_erofs_last_errno(), ENOTDIR);
    });
}

#[test]
fn stat_rejects_null_fs_path_or_attr() {
    let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
    let root = CString::new("/").unwrap();

    let rc = unsafe { fs_erofs_stat(std::ptr::null_mut(), root.as_ptr(), &mut attr) };
    assert_eq!(rc, -1);
    assert_eq!(fs_erofs_last_errno(), EINVAL);

    with_fixture(|fs| {
        let rc = unsafe { fs_erofs_stat(fs, std::ptr::null(), &mut attr) };
        assert_eq!(rc, -1, "NULL path must fail rather than deref");
        assert_eq!(fs_erofs_last_errno(), EINVAL);

        let rc = unsafe { fs_erofs_stat(fs, root.as_ptr(), std::ptr::null_mut()) };
        assert_eq!(rc, -1, "NULL attr must fail rather than be written through");
        assert_eq!(fs_erofs_last_errno(), EINVAL);
    });
}

#[test]
fn stat_leaves_attr_untouched_when_it_fails() {
    // A caller that ignores the return code shouldn't be able to mistake
    // stale stack bytes for a real answer -- but more importantly, the
    // driver must not partially fill the struct on the error path.
    with_fixture(|fs| {
        let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
        attr.inode = 0xDEAD_BEEF;
        attr.size = 0x1234;
        let c = CString::new("/no-such-file").unwrap();
        assert_eq!(unsafe { fs_erofs_stat(fs, c.as_ptr(), &mut attr) }, -1);
        assert_eq!(attr.inode, 0xDEAD_BEEF);
        assert_eq!(attr.size, 0x1234);
    });
}

#[test]
fn a_successful_call_clears_the_previous_failure() {
    // `last_error` / `last_errno` are only meaningful immediately after
    // a failed call. Every entry point clears them first, so a success
    // must wipe the record of the failure before it.
    with_fixture(|fs| {
        let bad = CString::new("/no-such-file").unwrap();
        let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { fs_erofs_stat(fs, bad.as_ptr(), &mut attr) }, -1);
        assert_eq!(fs_erofs_last_errno(), ENOENT);

        let good = CString::new("/hello.txt").unwrap();
        assert_eq!(unsafe { fs_erofs_stat(fs, good.as_ptr(), &mut attr) }, 0);
        assert_eq!(fs_erofs_last_errno(), 0, "stale errno survived a success");
        assert_eq!(capi_last_error(), "", "stale message survived a success");
    });
}

#[test]
fn mount_of_a_minimal_empty_image_still_works() {
    // Smallest thing mkfs will emit: a root directory with no children.
    // Guards against the ABI assuming at least one non-root inode.
    let bytes = mkfs::build_image(dir(vec![]), 12).unwrap();
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());
    let attr = stat_path(fs, "/").expect("stat / on an empty image");
    assert_eq!(attr.file_type, FT_DIR);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn two_mounts_of_the_same_image_are_independent() {
    // Handles are plain `Box`es; umounting one must not disturb the
    // other. This is the closest a test can get to proving the
    // free-once contract without invoking undefined behaviour.
    let img = temp_image(&capi_fixture_image());
    let a = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    let b = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!a.is_null() && !b.is_null());
    assert_ne!(a, b, "each mount owns a distinct handle");

    unsafe { fs_erofs_umount(a) };
    let attr = stat_path(b, "/hello.txt").expect("second handle survives the first umount");
    assert_eq!(attr.size, HELLO.len() as u64);
    unsafe { fs_erofs_umount(b) };
}

#[test]
fn big_file_attributes_match_the_bytes_written() {
    // Cross-check the fixture itself, so a size mismatch surfaces here
    // rather than as a confusing read failure in capi_read.rs.
    let body = big_body();
    assert_eq!(body.len(), BIG_LEN);
    let bytes = mkfs::build_image(dir(vec![("b", file(&body))]), 12).unwrap();
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null());
    assert_eq!(stat_path(fs, "/b").unwrap().size, BIG_LEN as u64);
    unsafe { fs_erofs_umount(fs) };
}
