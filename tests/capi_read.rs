//! File reads, symlinks and the alternative mount paths, through the C ABI.
//!
//! `capi_basic.rs` covers mounting, volume info and stat; `capi_dir.rs`
//! covers directory iteration. This file covers what was left: the two
//! read entry points, and the two ways of mounting that do not take a
//! filesystem path.
//!
//! Those mount paths matter more than their share of the code suggests.
//! `mount_with_fs_core_device` is how a caller mounts a *partition*
//! rather than a whole disk, and `mount_with_callbacks` is how it mounts
//! something that is not a file at all. A consumer that slices a
//! partition uses one of them for every mount it ever performs, so a
//! defect there is not an edge case.

mod common;

use common::errno::*;
use common::{
    big_body, capi_fixture_image, capi_last_error, long_link_target, temp_image, BIG_LEN, HELLO,
    LINK_TARGET, LONG_LINK_TARGET_LEN,
};
use fs_erofs::capi::*;
use std::ffi::{c_void, CString};

/// Mount the shared fixture, keeping the image alive for the caller.
fn mounted() -> (common::TempImage, *mut fs_erofs_fs_t) {
    let img = temp_image(&capi_fixture_image());
    let fs = unsafe { fs_erofs_mount(img.path.as_ptr()) };
    assert!(!fs.is_null(), "mount failed: {}", capi_last_error());
    (img, fs)
}

fn cpath(s: &str) -> CString {
    CString::new(s).unwrap()
}

// ---------------------------------------------------------------------
// fs_erofs_read_file
// ---------------------------------------------------------------------

#[test]
fn reads_a_whole_small_file() {
    let (_img, fs) = mounted();
    let mut buf = vec![0u8; 64];
    let n = unsafe {
        fs_erofs_read_file(
            fs,
            cpath("/hello.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n > 0, "read failed: {}", capi_last_error());
    assert_eq!(&buf[..n as usize], HELLO);
    unsafe { fs_erofs_umount(fs) };
}

/// A read from an offset must return the tail, not the head again.
#[test]
fn reads_from_an_offset() {
    let (_img, fs) = mounted();
    let mut buf = vec![0u8; 64];
    let skip = 6u64;
    let n = unsafe {
        fs_erofs_read_file(
            fs,
            cpath("/hello.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            skip,
            buf.len() as u64,
        )
    };
    assert_eq!(&buf[..n as usize], &HELLO[skip as usize..]);
    unsafe { fs_erofs_umount(fs) };
}

/// The big fixture spans two whole blocks plus a short tail, so these
/// reads cross a block boundary and end inside a partial one — the two
/// places a block-at-a-time reader gets the arithmetic wrong.
#[test]
fn reads_across_block_boundaries() {
    let (_img, fs) = mounted();
    let want = big_body();

    for &(off, len) in &[
        (0u64, BIG_LEN),         // the whole thing
        (0, 4096),               // exactly one block
        (4095, 2),               // straddling the first boundary
        (4096, 4096),            // the second whole block
        (8192, 100),             // the short tail alone
        (8000, 292),             // tail plus the end of the block before
        (BIG_LEN as u64 - 1, 1), // the final byte
    ] {
        let mut buf = vec![0u8; len];
        let n = unsafe {
            fs_erofs_read_file(
                fs,
                cpath("/big.bin").as_ptr(),
                buf.as_mut_ptr().cast::<c_void>(),
                off,
                len as u64,
            )
        };
        assert!(n >= 0, "read({off}, {len}) failed: {}", capi_last_error());
        let end = (off as usize + n as usize).min(want.len());
        assert_eq!(
            &buf[..n as usize],
            &want[off as usize..end],
            "read({off}, {len}) returned the wrong bytes"
        );
    }
    unsafe { fs_erofs_umount(fs) };
}

/// At and past end of file: zero bytes, and not an error. A caller loops
/// until it sees 0, so returning -1 here would look like a failure.
#[test]
fn reads_at_and_past_end_of_file_return_zero() {
    let (_img, fs) = mounted();
    let mut buf = vec![0u8; 32];
    for off in [HELLO.len() as u64, HELLO.len() as u64 + 1000] {
        let n = unsafe {
            fs_erofs_read_file(
                fs,
                cpath("/hello.txt").as_ptr(),
                buf.as_mut_ptr().cast::<c_void>(),
                off,
                buf.len() as u64,
            )
        };
        assert_eq!(n, 0, "a read starting at or past EOF must return 0");
    }
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn a_zero_length_read_is_not_an_error() {
    let (_img, fs) = mounted();
    let mut buf = [0u8; 1];
    let n = unsafe {
        fs_erofs_read_file(
            fs,
            cpath("/hello.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            0,
        )
    };
    assert_eq!(n, 0);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn reading_a_missing_path_reports_enoent() {
    let (_img, fs) = mounted();
    let mut buf = [0u8; 16];
    let n = unsafe {
        fs_erofs_read_file(
            fs,
            cpath("/not-here.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n, -1);
    assert_eq!(fs_erofs_last_errno(), ENOENT);
    assert!(!capi_last_error().is_empty());
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn reading_a_directory_is_refused() {
    let (_img, fs) = mounted();
    let mut buf = [0u8; 16];
    let n = unsafe {
        fs_erofs_read_file(
            fs,
            cpath("/sub").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(n, -1, "a directory's bytes are not file contents");
    assert_ne!(fs_erofs_last_errno(), 0);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn read_file_rejects_null_arguments() {
    let (_img, fs) = mounted();
    let mut buf = [0u8; 8];
    let p = cpath("/hello.txt");
    unsafe {
        assert_eq!(
            fs_erofs_read_file(
                std::ptr::null_mut(),
                p.as_ptr(),
                buf.as_mut_ptr().cast::<c_void>(),
                0,
                8
            ),
            -1
        );
        assert_eq!(
            fs_erofs_read_file(
                fs,
                std::ptr::null(),
                buf.as_mut_ptr().cast::<c_void>(),
                0,
                8
            ),
            -1
        );
        assert_eq!(
            fs_erofs_read_file(fs, p.as_ptr(), std::ptr::null_mut(), 0, 8),
            -1
        );
        fs_erofs_umount(fs);
    }
}

// ---------------------------------------------------------------------
// fs_erofs_readlink
// ---------------------------------------------------------------------

#[test]
fn reads_a_short_symlink_target() {
    let (_img, fs) = mounted();
    let mut buf = vec![0i8; 256];
    let n = unsafe { fs_erofs_readlink(fs, cpath("/link").as_ptr(), buf.as_mut_ptr(), buf.len()) };
    // This ABI returns 0 on success and writes a NUL-terminated target,
    // rather than returning the length.
    assert_eq!(n, 0, "readlink failed: {}", capi_last_error());
    let got = common::cchar_field_to_bytes(&buf);
    assert_eq!(String::from_utf8(got).unwrap(), LINK_TARGET);
    unsafe { fs_erofs_umount(fs) };
}

/// The long target does not fit in the inode's inline tail, so the
/// reader has to fetch it from a data block — a different code path.
#[test]
fn reads_a_long_symlink_target() {
    let (_img, fs) = mounted();
    let mut buf = vec![0i8; 512];
    let n =
        unsafe { fs_erofs_readlink(fs, cpath("/longlink").as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, 0, "readlink failed: {}", capi_last_error());
    let got = common::cchar_field_to_bytes(&buf);
    assert_eq!(got.len(), LONG_LINK_TARGET_LEN);
    assert_eq!(String::from_utf8(got).unwrap(), long_link_target());
    unsafe { fs_erofs_umount(fs) };
}

/// A buffer too small for the target is REFUSED rather than truncated.
///
/// That is this ABI's choice and it is the safer one: a silently
/// truncated path is a path to somewhere else, which a caller would
/// follow without noticing. Note the sibling XFS and Btrfs drivers
/// truncate instead — an inconsistency across the family worth
/// reconciling, but not by changing a shipped contract here.
#[test]
fn readlink_refuses_a_buffer_too_small_for_the_target() {
    let (_img, fs) = mounted();
    let mut buf = vec![0x7Fi8; 5];
    let n = unsafe { fs_erofs_readlink(fs, cpath("/link").as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, -1, "a target that does not fit must be refused");
    assert_eq!(
        fs_erofs_last_errno(),
        ERANGE,
        "a buffer too small is ERANGE, got {}",
        fs_erofs_last_errno()
    );
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn readlink_on_a_regular_file_is_refused() {
    let (_img, fs) = mounted();
    let mut buf = vec![0i8; 64];
    let n = unsafe {
        fs_erofs_readlink(
            fs,
            cpath("/hello.txt").as_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    assert_eq!(n, -1, "a regular file is not a symlink");
    assert_ne!(fs_erofs_last_errno(), 0);
    unsafe { fs_erofs_umount(fs) };
}

#[test]
fn readlink_rejects_null_and_empty_buffers() {
    let (_img, fs) = mounted();
    let p = cpath("/link");
    let mut buf = vec![0i8; 8];
    unsafe {
        assert_eq!(
            fs_erofs_readlink(std::ptr::null_mut(), p.as_ptr(), buf.as_mut_ptr(), 8),
            -1
        );
        assert_eq!(
            fs_erofs_readlink(fs, std::ptr::null(), buf.as_mut_ptr(), 8),
            -1
        );
        assert_eq!(
            fs_erofs_readlink(fs, p.as_ptr(), std::ptr::null_mut(), 8),
            -1
        );
        // Zero bytes leaves no room even for the terminator.
        assert!(fs_erofs_readlink(fs, p.as_ptr(), buf.as_mut_ptr(), 0) < 0);
        fs_erofs_umount(fs);
    }
}

// ---------------------------------------------------------------------
// The alternative mount paths
// ---------------------------------------------------------------------

struct ImageContext {
    bytes: Vec<u8>,
    /// Set to make every read fail, so a failure is proven to surface.
    fail: bool,
}

unsafe extern "C" fn ctx_read(
    context: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i32 {
    let ctx = unsafe { &*(context as *const ImageContext) };
    if ctx.fail {
        return -1;
    }
    let start = offset as usize;
    let end = start.saturating_add(length as usize);
    if end > ctx.bytes.len() {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            ctx.bytes[start..end].as_ptr(),
            buf.cast::<u8>(),
            end - start,
        )
    };
    0
}

fn cfg_for(ctx: Box<ImageContext>) -> fs_erofs_blockdev_cfg_t {
    let size = ctx.bytes.len() as u64;
    fs_erofs_blockdev_cfg_t {
        read: Some(ctx_read),
        context: Box::into_raw(ctx) as *mut c_void,
        size_bytes: size,
        block_size: 512,
    }
}

#[test]
fn mounts_over_a_caller_supplied_reader_and_reads_through_it() {
    let cfg = cfg_for(Box::new(ImageContext {
        bytes: capi_fixture_image(),
        fail: false,
    }));
    let fs = unsafe { fs_erofs_mount_with_callbacks(&cfg) };
    assert!(
        !fs.is_null(),
        "callback mount failed: {}",
        capi_last_error()
    );

    // Mounting is not enough; it has to actually serve reads.
    let mut buf = vec![0u8; 64];
    let n = unsafe {
        fs_erofs_read_file(
            fs,
            cpath("/hello.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert_eq!(&buf[..n as usize], HELLO);

    unsafe { fs_erofs_umount(fs) };
    drop(unsafe { Box::from_raw(cfg.context as *mut ImageContext) });
}

/// A reader that fails must surface as an error, never as silently
/// zeroed data — a caller cannot tell those apart.
#[test]
fn a_failing_callback_surfaces_as_an_error() {
    let cfg = cfg_for(Box::new(ImageContext {
        bytes: capi_fixture_image(),
        fail: true,
    }));
    let fs = unsafe { fs_erofs_mount_with_callbacks(&cfg) };
    assert!(fs.is_null(), "a failing reader must not produce a handle");
    assert!(!capi_last_error().is_empty());
    drop(unsafe { Box::from_raw(cfg.context as *mut ImageContext) });
}

#[test]
fn callback_mount_rejects_null_configuration() {
    unsafe {
        assert!(fs_erofs_mount_with_callbacks(std::ptr::null()).is_null());
        assert!(!capi_last_error().is_empty());
    }
    let cfg = fs_erofs_blockdev_cfg_t {
        read: None,
        context: std::ptr::null_mut(),
        size_bytes: 4096,
        block_size: 512,
    };
    assert!(unsafe { fs_erofs_mount_with_callbacks(&cfg) }.is_null());
}

#[test]
fn fs_core_device_mount_rejects_null() {
    assert!(unsafe { fs_erofs_mount_with_fs_core_device(std::ptr::null_mut()) }.is_null());
    assert!(!capi_last_error().is_empty());
}

/// `fs_erofs_last_error` must never return NULL, including before any
/// failure — a C caller prints it unconditionally.
#[test]
fn last_error_is_never_null() {
    // The pointer is what the header promises; the message is empty
    // until something has actually failed on this thread.
    assert!(!fs_erofs::capi::fs_erofs_last_error().is_null());
}
