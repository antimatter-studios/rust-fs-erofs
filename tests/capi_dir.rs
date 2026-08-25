//! C ABI directory iteration: `fs_erofs_dir_open` / `_dir_next` /
//! `_dir_close`.
//!
//! Called through the rlib for the reason documented in
//! `tests/capi_basic.rs`.

mod common;

use common::errno::*;
use common::{capi_fixture_image, capi_last_error, cchar_field_to_bytes, dir, file, temp_image};
use fs_erofs::capi::*;
use fs_erofs::mkfs;
use std::ffi::CString;

const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHR: u8 = 3;
const FT_BLK: u8 = 4;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_LNK: u8 = 7;

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    file_type: u8,
    inode: u64,
    name_len: u8,
}

fn with_fixture(body: impl FnOnce(*mut fs_erofs_fs_t)) {
    let img = temp_image(&capi_fixture_image());
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "mount failed: {}", capi_last_error());
    body(fs);
    unsafe { fs_erofs_umount(fs) };
}

fn open_dir(fs: *mut fs_erofs_fs_t, path: &str) -> *mut fs_erofs_dir_iter_t {
    let c = CString::new(path).unwrap();
    unsafe { fs_erofs_dir_open(fs, c.as_ptr()) }
}

/// Drain an iterator, decoding each dirent through the same NUL-scan a C
/// caller would use.
fn drain(iter: *mut fs_erofs_dir_iter_t) -> Vec<Entry> {
    let mut out = Vec::new();
    loop {
        let p = unsafe { fs_erofs_dir_next(iter) };
        if p.is_null() {
            break;
        }
        let e = unsafe { *p };
        let bytes = cchar_field_to_bytes(&e.name);
        out.push(Entry {
            name: String::from_utf8(bytes).expect("fixture names are ASCII"),
            file_type: e.file_type,
            inode: e.inode,
            name_len: e.name_len,
        });
    }
    out
}

// ---- listing -----------------------------------------------------------

#[test]
fn root_listing_omits_dot_and_dotdot() {
    // EROFS stores "." and ".." on disk, but FSKit (and every other
    // consumer of this ABI) synthesises them itself. Leaking them here
    // would show duplicate rows in a file browser.
    with_fixture(|fs| {
        let iter = open_dir(fs, "/");
        assert!(!iter.is_null(), "{}", capi_last_error());
        let names: Vec<String> = drain(iter).into_iter().map(|e| e.name).collect();
        unsafe { fs_erofs_dir_close(iter) };

        assert!(!names.contains(&".".to_string()), "'.' leaked: {names:?}");
        assert!(!names.contains(&"..".to_string()), "'..' leaked: {names:?}");

        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "big.bin",
                "blk",
                "chr",
                "empty.bin",
                "fifo",
                "hello.txt",
                "link",
                "longlink",
                "owned.txt",
                "sock",
                "sub",
            ]
        );
    });
}

#[test]
fn dirent_file_types_cover_every_abi_value() {
    with_fixture(|fs| {
        let iter = open_dir(fs, "/");
        let entries = drain(iter);
        unsafe { fs_erofs_dir_close(iter) };

        let ty = |n: &str| {
            entries
                .iter()
                .find(|e| e.name == n)
                .unwrap_or_else(|| panic!("missing {n}"))
                .file_type
        };
        assert_eq!(ty("hello.txt"), FT_REG);
        assert_eq!(ty("sub"), FT_DIR);
        assert_eq!(ty("chr"), FT_CHR);
        assert_eq!(ty("blk"), FT_BLK);
        assert_eq!(ty("fifo"), FT_FIFO);
        assert_eq!(ty("sock"), FT_SOCK);
        assert_eq!(ty("link"), FT_LNK);
    });
}

#[test]
fn dirent_inode_matches_what_stat_reports_for_the_same_path() {
    // A consumer builds its file-ID table from dirents and then stats
    // individual paths; if the two disagreed, the same file would get
    // two identities.
    with_fixture(|fs| {
        let iter = open_dir(fs, "/");
        let entries = drain(iter);
        unsafe { fs_erofs_dir_close(iter) };

        for e in &entries {
            let path = CString::new(format!("/{}", e.name)).unwrap();
            let mut attr: fs_erofs_attr_t = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { fs_erofs_stat(fs, path.as_ptr(), &mut attr) }, 0);
            assert_eq!(attr.inode, e.inode, "NID mismatch for {}", e.name);
        }
    });
}

#[test]
fn dirent_name_len_matches_the_nul_terminated_name() {
    with_fixture(|fs| {
        let iter = open_dir(fs, "/");
        for e in drain(iter) {
            assert_eq!(
                e.name_len as usize,
                e.name.len(),
                "name_len disagrees with the string for {}",
                e.name
            );
        }
        unsafe { fs_erofs_dir_close(iter) };
    });
}

#[test]
fn nested_directories_open_by_path() {
    with_fixture(|fs| {
        let iter = open_dir(fs, "/sub");
        assert!(!iter.is_null(), "{}", capi_last_error());
        let mut names: Vec<String> = drain(iter).into_iter().map(|e| e.name).collect();
        unsafe { fs_erofs_dir_close(iter) };
        names.sort();
        assert_eq!(names, vec!["deeper", "nested.txt"]);

        let iter = open_dir(fs, "/sub/deeper");
        let names: Vec<String> = drain(iter).into_iter().map(|e| e.name).collect();
        unsafe { fs_erofs_dir_close(iter) };
        assert_eq!(names, vec!["leaf.txt"]);
    });
}

// ---- iteration contract ------------------------------------------------

#[test]
fn iterating_to_completion_ends_cleanly_and_stays_ended() {
    // NULL from `_dir_next` means "end", not "error": the errno must
    // still read 0, and further calls must keep returning NULL rather
    // than wrapping around or walking off the end of the vector.
    with_fixture(|fs| {
        let iter = open_dir(fs, "/");
        assert_eq!(fs_erofs_last_errno(), 0, "dir_open clears the error slots");
        let n = drain(iter).len();
        assert_eq!(n, 11, "fixture root has 11 visible entries");
        assert_eq!(
            fs_erofs_last_errno(),
            0,
            "end-of-iteration must not look like a failure"
        );
        for _ in 0..3 {
            assert!(
                unsafe { fs_erofs_dir_next(iter) }.is_null(),
                "iteration must stay ended"
            );
        }
        unsafe { fs_erofs_dir_close(iter) };
    });
}

#[test]
fn an_empty_directory_ends_on_the_very_first_next() {
    // On disk it still holds "." and "..", both filtered out, so the
    // first `_dir_next` has to report end rather than the two synthetic
    // names.
    let bytes = mkfs::build_image(dir(vec![("d", dir(vec![]))]), 12).unwrap();
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());

    let iter = open_dir(fs, "/d");
    assert!(!iter.is_null(), "{}", capi_last_error());
    assert!(unsafe { fs_erofs_dir_next(iter) }.is_null());
    unsafe { fs_erofs_dir_close(iter) };
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn dir_next_returns_the_same_buffer_each_time_with_fresh_contents() {
    // The documented contract: the returned pointer aims into the
    // iterator's own buffer and is valid only until the next call. So
    // the address must be stable and the contents must be overwritten
    // in place -- a caller that keeps the pointer sees entry N+1.
    with_fixture(|fs| {
        let iter = open_dir(fs, "/");
        let p1 = unsafe { fs_erofs_dir_next(iter) };
        assert!(!p1.is_null());
        let first = unsafe { *p1 };
        let p2 = unsafe { fs_erofs_dir_next(iter) };
        assert!(!p2.is_null());
        assert_eq!(p1, p2, "dirent buffer address must be stable");
        let second = unsafe { *p2 };
        assert_ne!(
            cchar_field_to_bytes(&first.name),
            cchar_field_to_bytes(&second.name),
            "the shared buffer was not refreshed"
        );
        unsafe { fs_erofs_dir_close(iter) };
    });
}

#[test]
fn two_iterators_over_the_same_directory_advance_independently() {
    with_fixture(|fs| {
        let a = open_dir(fs, "/");
        let b = open_dir(fs, "/");
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);

        let first_a = unsafe { *fs_erofs_dir_next(a) };
        let first_b = unsafe { *fs_erofs_dir_next(b) };
        assert_eq!(
            cchar_field_to_bytes(&first_a.name),
            cchar_field_to_bytes(&first_b.name),
            "a fresh iterator restarts at entry 0"
        );

        // Drain `a` completely; `b` must be unaffected.
        while !unsafe { fs_erofs_dir_next(a) }.is_null() {}
        assert!(
            !unsafe { fs_erofs_dir_next(b) }.is_null(),
            "draining one iterator ended the other"
        );
        unsafe { fs_erofs_dir_close(a) };
        unsafe { fs_erofs_dir_close(b) };
    });
}

#[test]
fn a_wide_directory_yields_every_entry_exactly_once() {
    // 300 entries spill across several 4 KiB directory blocks, so this
    // covers the multi-block read path behind `_dir_open` as well as
    // the iterator's own bookkeeping.
    let names: Vec<String> = (0..300).map(|i| format!("f{i:03}.txt")).collect();
    let entries: Vec<(&str, mkfs::Node)> = names
        .iter()
        .map(|n| (n.as_str(), file(n.as_bytes())))
        .collect();
    let bytes = mkfs::build_image(dir(entries), 12).unwrap();
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());

    let iter = open_dir(fs, "/");
    let mut got: Vec<String> = drain(iter).into_iter().map(|e| e.name).collect();
    unsafe { fs_erofs_dir_close(iter) };
    got.sort();
    let mut want = names.clone();
    want.sort();
    assert_eq!(got.len(), 300, "entry count");
    assert_eq!(got, want);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn a_name_filling_the_dirent_buffer_stays_nul_terminated() {
    // `fs_erofs_dirent_t.name` is char[256]; 255 bytes is the largest
    // name that leaves room for the terminator, and it is Linux's
    // NAME_MAX. The last array slot must hold the NUL, not a name byte.
    let long = "x".repeat(255);
    let bytes = mkfs::build_image(dir(vec![(long.as_str(), file(b"n"))]), 12).unwrap();
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());

    let iter = open_dir(fs, "/");
    let entries = drain(iter);
    unsafe { fs_erofs_dir_close(iter) };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, long);
    assert_eq!(entries[0].name_len, 255);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn non_ascii_names_pass_through_byte_for_byte() {
    // The ABI declares `char name[256]` and makes no encoding promise
    // beyond "whatever mkfs stored". Round-trip UTF-8 to prove no
    // lossy conversion happens on the way out.
    let name = "ünïcødé-ファイル.txt";
    let bytes = mkfs::build_image(dir(vec![(name, file(b"u"))]), 12).unwrap();
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());

    let iter = open_dir(fs, "/");
    let entries = drain(iter);
    unsafe { fs_erofs_dir_close(iter) };
    assert_eq!(entries[0].name, name);
    assert_eq!(entries[0].name_len as usize, name.len());
    unsafe { fs_erofs_umount(fs) };
}

// ---- failure modes -----------------------------------------------------

#[test]
fn dir_open_on_a_regular_file_reports_enotdir() {
    with_fixture(|fs| {
        let iter = open_dir(fs, "/hello.txt");
        assert!(iter.is_null());
        assert_eq!(fs_erofs_last_errno(), ENOTDIR);
        assert!(
            capi_last_error().contains("not a directory"),
            "got: {}",
            capi_last_error()
        );
    });
}

#[test]
fn dir_open_on_a_symlink_reports_enotdir() {
    // `_dir_open` resolves with `lookup_path`, which does not follow
    // symlinks -- so even a symlink pointing at a directory is not
    // itself openable. Pinned so the no-follow policy stays consistent
    // with `fs_erofs_stat`.
    with_fixture(|fs| {
        let iter = open_dir(fs, "/link");
        assert!(iter.is_null());
        assert_eq!(fs_erofs_last_errno(), ENOTDIR);
    });
}

#[test]
fn dir_open_on_a_missing_path_reports_enoent() {
    with_fixture(|fs| {
        let iter = open_dir(fs, "/no-such-dir");
        assert!(iter.is_null());
        assert_eq!(fs_erofs_last_errno(), ENOENT);
        assert!(
            capi_last_error().contains("/no-such-dir"),
            "got: {}",
            capi_last_error()
        );
    });
}

#[test]
fn dir_open_through_a_non_directory_reports_enotdir() {
    with_fixture(|fs| {
        let iter = open_dir(fs, "/hello.txt/inner");
        assert!(iter.is_null());
        assert_eq!(fs_erofs_last_errno(), ENOTDIR);
    });
}

#[test]
fn dir_open_rejects_null_fs_and_null_path() {
    let root = CString::new("/").unwrap();
    let iter = unsafe { fs_erofs_dir_open(std::ptr::null_mut(), root.as_ptr()) };
    assert!(iter.is_null());
    assert_eq!(fs_erofs_last_errno(), EINVAL);

    with_fixture(|fs| {
        let iter = unsafe { fs_erofs_dir_open(fs, std::ptr::null()) };
        assert!(iter.is_null(), "NULL path must fail rather than deref");
        assert_eq!(fs_erofs_last_errno(), EINVAL);
    });
}

#[test]
fn dir_next_and_dir_close_tolerate_null() {
    // `_dir_open` returns NULL on failure; a caller that forgets to
    // check it will pass that NULL straight into `_dir_next` and then
    // `_dir_close`. Neither may dereference it.
    assert!(unsafe { fs_erofs_dir_next(std::ptr::null_mut()) }.is_null());
    unsafe { fs_erofs_dir_close(std::ptr::null_mut()) };
}

#[test]
fn dir_open_on_the_empty_path_lists_the_root() {
    // Same empty-path-means-root rule as `fs_erofs_stat`; recorded here
    // so the two entry points can't drift apart.
    with_fixture(|fs| {
        let iter = open_dir(fs, "");
        assert!(!iter.is_null(), "{}", capi_last_error());
        assert_eq!(drain(iter).len(), 11);
        unsafe { fs_erofs_dir_close(iter) };
    });
}

#[test]
fn dir_open_reports_a_corrupt_directory_as_eio() {
    // Scribble over the root's dirent block so `read_dir` fails after
    // the inode itself parsed fine. A structurally broken directory is
    // neither "missing" nor "not a directory" -- it is a damaged
    // volume, which is the EIO bucket.
    let mut bytes = capi_fixture_image();
    let root_data_off = {
        let fs = common::open_image(bytes.clone());
        let root = fs.root_inode().unwrap();
        // Dirent block 0: `nameoff` of the first entry lives at byte 8
        // of the block. Point it past the end of the block to force
        // `BadDirent` rather than a plausible-looking listing.
        fs_erofs::inode::Inode::iloc(fs.superblock(), root.nid) as usize
    };
    // Corrupt the root inode's raw_u (data pointer, inode offset 0x10)
    // so its dirent blocks resolve to garbage well past the image end.
    bytes[root_data_off + 0x10..root_data_off + 0x14]
        .copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
    let img = temp_image(&bytes);
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "{}", capi_last_error());
    let iter = open_dir(fs, "/");
    assert!(iter.is_null(), "a directory pointing off-image must fail");
    assert_eq!(fs_erofs_last_errno(), EIO);
    assert!(!capi_last_error().is_empty());
    unsafe { fs_erofs_umount(fs) };
}
