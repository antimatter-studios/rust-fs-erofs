//! C ABI exports — MUST match `include/fs_erofs.h` exactly. Consumers link
//! `libfs_erofs.a` and `#include` that header; any signature change here
//! requires the header to change in lockstep.
//!
//! Read-only surface (EROFS is an inherently read-only filesystem):
//! - fs_erofs_mount(device_path) -> *mut fs_erofs_fs_t
//! - fs_erofs_mount_with_callbacks(cfg) -> *mut fs_erofs_fs_t
//! - fs_erofs_mount_with_fs_core_device(handle) -> *mut fs_erofs_fs_t
//! - fs_erofs_umount(fs)
//! - fs_erofs_get_volume_info(fs, info) -> int
//! - fs_erofs_stat(fs, path, attr) -> int
//! - fs_erofs_dir_open(fs, path) / _dir_next(iter) / _dir_close(iter)
//! - fs_erofs_read_file(fs, path, buf, offset, length) -> int64
//! - fs_erofs_readlink(fs, path, buf, bufsize) -> int
//! - fs_erofs_last_error() -> *const c_char
//! - fs_erofs_last_errno() -> c_int
//!
//! Memory ownership mirrors the sister fs-* crates: `fs_erofs_fs_t*` freed
//! via `fs_erofs_umount`; `fs_erofs_dir_iter_t*` freed via `_dir_close`;
//! `_dir_next` returns a pointer into the iterator's buffer valid until the
//! next `_dir_next`/`_dir_close`; `_last_error`/`_last_errno` are
//! thread-local, valid until the next FFI call on the same thread.

#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use crate::error::Error;
use crate::fs::Filesystem;
use crate::inode::Inode;
use fs_core::callback_device::CallbackDevice;
use fs_core::BlockRead;

// POSIX errno values surfaced through the C ABI (hand-rolled to avoid a
// libc dependency just to name a handful of constants).
const EIO: c_int = 5;
const ENOENT: c_int = 2;
const ENOTDIR: c_int = 20;
const EINVAL: c_int = 22;
const ERANGE: c_int = 34;

fn errno_for(e: &Error) -> c_int {
    match e {
        Error::NotFound => ENOENT,
        Error::NotADirectory => ENOTDIR,
        Error::OutOfRange => ERANGE,
        Error::NotErofs | Error::BadSuperblock(_) => EINVAL,
        _ => EIO,
    }
}

// ===========================================================================
// Thread-local last error (message + POSIX errno)
// ===========================================================================

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
    static LAST_ERRNO: RefCell<c_int> = const { RefCell::new(0) };
}

fn set_last_error<E: std::fmt::Display>(e: E) {
    let msg = format!("{e}");
    LAST_ERROR.with(|c| {
        *c.borrow_mut() =
            CString::new(msg).unwrap_or_else(|_| CString::new("unknown error").unwrap());
    });
}

fn set_err_from(err: &Error, context: &str) {
    set_last_error(format!("{context}: {err}"));
    LAST_ERRNO.with(|c| *c.borrow_mut() = errno_for(err));
}

fn set_err_msg(msg: &str, e: c_int) {
    set_last_error(msg);
    LAST_ERRNO.with(|c| *c.borrow_mut() = e);
}

fn clear_last_error() {
    LAST_ERROR.with(|c| *c.borrow_mut() = CString::new("").unwrap());
    LAST_ERRNO.with(|c| *c.borrow_mut() = 0);
}

fn ffi_guard<T>(fail: T, body: impl FnOnce() -> T + std::panic::UnwindSafe) -> T {
    match std::panic::catch_unwind(body) {
        Ok(v) => v,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&'static str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic (non-string payload)".to_string());
            set_err_msg(&format!("panic: {msg}"), EIO);
            fail
        }
    }
}

#[no_mangle]
pub extern "C" fn fs_erofs_last_error() -> *const c_char {
    LAST_ERROR.with(|c| c.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn fs_erofs_last_errno() -> c_int {
    LAST_ERRNO.with(|c| *c.borrow())
}

unsafe fn cstr_to_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

// ===========================================================================
// Opaque handles + C structs
// ===========================================================================

pub struct fs_erofs_fs_t {
    fs: Filesystem,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct fs_erofs_attr_t {
    /// EROFS inode number (NID) — 64-bit.
    pub inode: u64,
    pub mode: u16, // permission bits (no type bits)
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime: u32,
    pub link_count: u32,
    pub file_type: u32, // fs_erofs_file_type_t
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct fs_erofs_dirent_t {
    pub inode: u64,
    pub file_type: u8,
    pub name_len: u8,
    pub name: [c_char; 256],
}

#[repr(C)]
pub struct fs_erofs_volume_info_t {
    pub block_size: u32,
    pub total_blocks: u32,
    pub inode_count: u64,
    pub build_time: u64,
    pub volume_name: [c_char; 16],
    pub uuid: [u8; 16],
    pub feature_compat: u32,
    pub feature_incompat: u32,
}

pub type fs_erofs_read_fn = Option<
    unsafe extern "C" fn(context: *mut c_void, buf: *mut c_void, offset: u64, length: u64) -> c_int,
>;

#[repr(C)]
pub struct fs_erofs_blockdev_cfg_t {
    pub read: fs_erofs_read_fn,
    pub context: *mut c_void,
    pub size_bytes: u64,
    pub block_size: u32,
}

pub struct fs_erofs_dir_iter_t {
    entries: Vec<fs_erofs_dirent_t>,
    position: usize,
    current: fs_erofs_dirent_t,
}

// ===========================================================================
// Helpers
// ===========================================================================

// Standard S_IFMT → ABI file-type byte (UNKNOWN=0, REG=1, DIR=2, CHR=3,
// BLK=4, FIFO=5, SOCK=6, LNK=7). EROFS stores a POSIX mode; its dirent
// type byte already uses this same encoding, so directory entries pass
// through unchanged.
fn mode_to_abi(mode: u16) -> u8 {
    match mode & 0o170000 {
        0o100000 => 1, // S_IFREG
        0o040000 => 2, // S_IFDIR
        0o020000 => 3, // S_IFCHR
        0o060000 => 4, // S_IFBLK
        0o010000 => 5, // S_IFIFO
        0o140000 => 6, // S_IFSOCK
        0o120000 => 7, // S_IFLNK
        _ => 0,
    }
}

fn fill_attr(out: &mut fs_erofs_attr_t, inode: &Inode) {
    out.inode = inode.nid;
    out.mode = inode.mode & 0o7777;
    out.uid = inode.uid;
    out.gid = inode.gid;
    out.size = inode.size;
    out.mtime = inode.mtime as u32;
    out.link_count = inode.nlink;
    out.file_type = mode_to_abi(inode.mode) as u32;
}

fn dir_entry_to_abi(e: &crate::dir::DirEntry) -> fs_erofs_dirent_t {
    let mut name = [0 as c_char; 256];
    let copy = e.name.len().min(255);
    for (i, &b) in e.name[..copy].iter().enumerate() {
        name[i] = b as c_char;
    }
    name[copy] = 0;
    fs_erofs_dirent_t {
        inode: e.nid,
        file_type: e.file_type,
        name_len: copy as u8,
        name,
    }
}

fn mount_from_device(dev: Arc<dyn BlockRead>, context: &str) -> *mut fs_erofs_fs_t {
    match Filesystem::open(dev) {
        Ok(fs) => Box::into_raw(Box::new(fs_erofs_fs_t { fs })),
        Err(e) => {
            set_err_from(&e, context);
            std::ptr::null_mut()
        }
    }
}

// ===========================================================================
// Lifecycle
// ===========================================================================

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_mount(device_path: *const c_char) -> *mut fs_erofs_fs_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            let path = unsafe { cstr_to_str(device_path) };
            if path.is_empty() {
                set_err_msg("null or empty device_path", EINVAL);
                return std::ptr::null_mut();
            }
            let dev = match fs_core::FileDevice::open(path) {
                Ok(d) => Arc::new(d) as Arc<dyn BlockRead>,
                Err(e) => {
                    set_err_msg(&format!("open {path}: {e}"), EIO);
                    return std::ptr::null_mut();
                }
            };
            mount_from_device(dev, &format!("mount {path}"))
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_mount_with_callbacks(
    cfg: *const fs_erofs_blockdev_cfg_t,
) -> *mut fs_erofs_fs_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            if cfg.is_null() {
                set_err_msg("null cfg", EINVAL);
                return std::ptr::null_mut();
            }
            let cfg = unsafe { &*cfg };
            let Some(read_fn) = cfg.read else {
                set_err_msg("cfg.read is null", EINVAL);
                return std::ptr::null_mut();
            };
            let ctx_addr = cfg.context as usize;
            let dev = CallbackDevice {
                size: cfg.size_bytes,
                read: Box::new(move |offset, buf| {
                    let rc = unsafe {
                        read_fn(
                            ctx_addr as *mut c_void,
                            buf.as_mut_ptr() as *mut c_void,
                            offset,
                            buf.len() as u64,
                        )
                    };
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::other(format!(
                            "read callback returned {rc}"
                        )))
                    }
                }),
                write: None,
                flush: None,
            };
            mount_from_device(Arc::new(dev) as Arc<dyn BlockRead>, "mount (callback)")
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_mount_with_fs_core_device(
    handle: *mut fs_core::ffi::FsCoreDevice,
) -> *mut fs_erofs_fs_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            if handle.is_null() {
                set_err_msg("null fs_core handle", EINVAL);
                return std::ptr::null_mut();
            }
            // EROFS is read-only; the read half of the device trait is all
            // we need. Trait upcast supported on the pinned toolchain.
            let dev: Arc<dyn fs_core::BlockDevice> = unsafe { (*handle).inner().clone() };
            let read: Arc<dyn BlockRead> = dev;
            mount_from_device(read, "mount via fs_core handle")
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_umount(fs: *mut fs_erofs_fs_t) {
    ffi_guard(
        (),
        AssertUnwindSafe(|| {
            if !fs.is_null() {
                drop(unsafe { Box::from_raw(fs) });
            }
        }),
    )
}

// ===========================================================================
// Volume info / stat / readdir / read / readlink
// ===========================================================================

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_get_volume_info(
    fs: *mut fs_erofs_fs_t,
    info: *mut fs_erofs_volume_info_t,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || info.is_null() {
                set_err_msg("null fs or info", EINVAL);
                return -1;
            }
            let fs = unsafe { &(*fs).fs };
            let info = unsafe { &mut *info };
            unsafe { std::ptr::write_bytes(info as *mut fs_erofs_volume_info_t, 0, 1) };

            let sb = fs.superblock();
            info.block_size = sb.block_size() as u32;
            info.total_blocks = sb.blocks;
            info.inode_count = sb.inos;
            info.build_time = sb.build_time;
            let name = sb.volume_name_str().as_bytes();
            let n = name.len().min(15);
            for (i, &b) in name[..n].iter().enumerate() {
                info.volume_name[i] = b as c_char;
            }
            info.volume_name[n] = 0;
            info.uuid = sb.uuid;
            info.feature_compat = sb.feature_compat;
            info.feature_incompat = sb.feature_incompat;
            0
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_stat(
    fs: *mut fs_erofs_fs_t,
    path: *const c_char,
    attr: *mut fs_erofs_attr_t,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || attr.is_null() {
                set_err_msg("null fs, path, or attr", EINVAL);
                return -1;
            }
            let fs = unsafe { &(*fs).fs };
            let path = unsafe { cstr_to_str(path) };
            let attr = unsafe { &mut *attr };
            match fs.lookup_path(path) {
                Ok(inode) => {
                    fill_attr(attr, &inode);
                    0
                }
                Err(e) => {
                    set_err_from(&e, &format!("stat {path}"));
                    -1
                }
            }
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_dir_open(
    fs: *mut fs_erofs_fs_t,
    path: *const c_char,
) -> *mut fs_erofs_dir_iter_t {
    ffi_guard(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() {
                set_err_msg("null fs or path", EINVAL);
                return std::ptr::null_mut();
            }
            let fs = unsafe { &(*fs).fs };
            let path = unsafe { cstr_to_str(path) };

            let inode = match fs.lookup_path(path) {
                Ok(i) => i,
                Err(e) => {
                    set_err_from(&e, &format!("dir_open {path}"));
                    return std::ptr::null_mut();
                }
            };
            if !inode.is_dir() {
                set_err_msg(&format!("dir_open {path}: not a directory"), ENOTDIR);
                return std::ptr::null_mut();
            }
            let entries = match fs.read_dir(&inode) {
                Ok(es) => es
                    .iter()
                    // EROFS includes "." and ".." in its on-disk listing;
                    // FSKit synthesizes those, so drop them here.
                    .filter(|e| e.name != b"." && e.name != b"..")
                    .map(dir_entry_to_abi)
                    .collect(),
                Err(e) => {
                    set_err_from(&e, &format!("read directory {path}"));
                    return std::ptr::null_mut();
                }
            };
            Box::into_raw(Box::new(fs_erofs_dir_iter_t {
                entries,
                position: 0,
                current: unsafe { std::mem::zeroed() },
            }))
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_dir_next(
    iter: *mut fs_erofs_dir_iter_t,
) -> *const fs_erofs_dirent_t {
    ffi_guard(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            if iter.is_null() {
                return std::ptr::null();
            }
            let iter = unsafe { &mut *iter };
            if iter.position >= iter.entries.len() {
                return std::ptr::null();
            }
            iter.current = iter.entries[iter.position];
            iter.position += 1;
            &iter.current as *const fs_erofs_dirent_t
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_dir_close(iter: *mut fs_erofs_dir_iter_t) {
    ffi_guard(
        (),
        AssertUnwindSafe(|| {
            if !iter.is_null() {
                drop(unsafe { Box::from_raw(iter) });
            }
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_read_file(
    fs: *mut fs_erofs_fs_t,
    path: *const c_char,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i64 {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || buf.is_null() {
                set_err_msg("null fs, path, or buf", EINVAL);
                return -1;
            }
            let fs = unsafe { &(*fs).fs };
            let path = unsafe { cstr_to_str(path) };

            let inode = match fs.lookup_path(path) {
                Ok(i) => i,
                Err(e) => {
                    set_err_from(&e, &format!("read_file {path}"));
                    return -1;
                }
            };
            if !inode.is_regular_file() {
                set_err_msg(&format!("read_file {path}: not a regular file"), EINVAL);
                return -1;
            }
            if offset >= inode.size {
                return 0;
            }
            // erofs read_file fills the buffer EXACTLY, erroring if it would
            // read past EOF — so size the read to what's actually available.
            let avail = inode.size - offset;
            let to_read = length.min(avail).min(usize::MAX as u64) as usize;
            if to_read == 0 {
                return 0;
            }
            let out = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, to_read) };
            match fs.read_file(&inode, offset, out) {
                Ok(()) => to_read as i64,
                Err(e) => {
                    set_err_from(&e, &format!("read_file {path}"));
                    -1
                }
            }
        }),
    )
}

#[no_mangle]
pub unsafe extern "C" fn fs_erofs_readlink(
    fs: *mut fs_erofs_fs_t,
    path: *const c_char,
    buf: *mut c_char,
    bufsize: usize,
) -> c_int {
    ffi_guard(
        -1,
        AssertUnwindSafe(|| {
            clear_last_error();
            if fs.is_null() || path.is_null() || buf.is_null() || bufsize == 0 {
                set_err_msg("null fs/path/buf or zero bufsize", EINVAL);
                return -1;
            }
            let fs = unsafe { &(*fs).fs };
            let path = unsafe { cstr_to_str(path) };

            let inode = match fs.lookup_path(path) {
                Ok(i) => i,
                Err(e) => {
                    set_err_from(&e, &format!("readlink {path}"));
                    return -1;
                }
            };
            if !inode.is_symlink() {
                set_err_msg(&format!("readlink {path}: not a symlink"), EINVAL);
                return -1;
            }
            let target = match fs.read_symlink_target(&inode) {
                Ok(t) => t,
                Err(e) => {
                    set_err_from(&e, &format!("readlink {path}"));
                    return -1;
                }
            };
            if target.len() + 1 > bufsize {
                set_err_msg("readlink buffer too small", ERANGE);
                return -1;
            }
            let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, bufsize) };
            dst[..target.len()].copy_from_slice(&target);
            dst[target.len()] = 0;
            0
        }),
    )
}
