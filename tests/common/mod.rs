//! Shared helpers for integration tests.
//!
//! Lives in `tests/common/mod.rs` so every `tests/*.rs` integration
//! file can `mod common;` and reuse it without each test crate getting
//! its own duplicate copy.
//!
//! Each integration test compiles `common/` independently, and uses
//! only a subset of the helpers, so most files have a few "unused"
//! warnings -- silenced at module scope rather than per-item.

#![allow(dead_code)]

use fs_core::BlockRead;
use fs_erofs::{mkfs, Filesystem};
use fs_erofs_test_support::{oracle, ScratchDir};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// In-memory `BlockRead` impl backed by a `Vec<u8>`. Owned via
/// `Mutex<Vec<u8>>` so the device is `Send + Sync`.
pub struct MemDev(Mutex<Vec<u8>>);

impl MemDev {
    pub fn new(bytes: Vec<u8>) -> Self {
        MemDev(Mutex::new(bytes))
    }

    /// Construct an `Arc<dyn BlockRead>` from raw bytes -- the shape
    /// `Filesystem::open` wants.
    pub fn arc(bytes: Vec<u8>) -> Arc<dyn BlockRead> {
        Arc::new(MemDev::new(bytes))
    }
}

impl BlockRead for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let v = self.0.lock().unwrap();
        let s = offset as usize;
        let e = s + buf.len();
        if e > v.len() {
            return Err(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: v.len().saturating_sub(s),
            });
        }
        buf.copy_from_slice(&v[s..e]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
}

/// Open an EROFS image given as raw bytes. Panics on parse failure --
/// integration tests are expected to feed valid images here.
pub fn open_image(bytes: Vec<u8>) -> Filesystem {
    Filesystem::open(MemDev::arc(bytes)).expect("filesystem open")
}

/// Open an EROFS image from a file path.
pub fn open_image_path(path: &Path) -> Filesystem {
    let bytes = std::fs::read(path).expect("read image file");
    open_image(bytes)
}

// ---- mkfs::Node tree builders -----------------------------------------

/// Build a `mkfs::Node::Dir` from a `(name, child)` slice.
pub fn dir(entries: Vec<(&str, mkfs::Node)>) -> mkfs::Node {
    let mut m = BTreeMap::new();
    for (k, v) in entries {
        m.insert(k.to_string(), v);
    }
    mkfs::Node::Dir {
        mode: mkfs::DEFAULT_DIR_MODE,
        entries: m,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// Build a `mkfs::Node::File` from a byte slice.
pub fn file(data: &[u8]) -> mkfs::Node {
    mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

// ---- erofs-utils oracle plumbing --------------------------------------
//
// THE TOOLS RUN IN THE HARNESS VM AND NOWHERE ELSE. What used to be
// here was a `tool_available()` probe and three `*_available()` helpers
// whose callers returned early when the answer was no: on a machine
// without erofs-utils the oracle suites passed having compared this
// driver against nothing, and the only thing standing between that and
// CI was an assert keyed on the `CI` environment variable. Measured on
// this host, `mkfs.erofs` was present and was version 1.5, which does
// not answer `-V` at all -- so the probe said "absent", every oracle
// test returned early, and `a_freshly_built_zstd_image_reads_back_exactly`
// reported ok having built no image.
//
// There is now one way in -- fs_erofs_test_support::oracle -- it runs
// the tool in the guest where erofs-utils 1.9.1 is built from source,
// and it FAILS when it cannot. tests/test_contract.rs refuses any other
// shape.

/// Materialize a `mkfs::Node` tree onto disk under `root`. Used to
/// stage a source tree for `mkfs.erofs` to ingest.
pub fn materialize_tree(root: &Path, node: &mkfs::Node) {
    match node {
        mkfs::Node::Dir { entries, .. } => {
            std::fs::create_dir_all(root).expect("create dir");
            for (name, child) in entries {
                let p = root.join(name);
                materialize_node(&p, child);
            }
        }
        mkfs::Node::File { data, .. } => {
            // Top-level "tree" is a file -- write it directly. Unusual
            // but supported for symmetry.
            let mut f = std::fs::File::create(root).expect("create file");
            f.write_all(data).expect("write");
        }
        mkfs::Node::Symlink { target, .. } => {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, root).expect("create symlink");
            #[cfg(not(unix))]
            {
                let _ = target;
                panic!("symlink materialization only supported on unix");
            }
        }
        mkfs::Node::Device { .. }
        | mkfs::Node::Special { .. }
        | mkfs::Node::ChunkedFile { .. }
        | mkfs::Node::CompressedFile(_) => {
            panic!("materialize_tree: non-regular Node kinds aren't supported in oracle staging");
        }
    }
}

fn materialize_node(path: &Path, node: &mkfs::Node) {
    match node {
        mkfs::Node::Dir { entries, .. } => {
            std::fs::create_dir_all(path).expect("create dir");
            for (name, child) in entries {
                materialize_node(&path.join(name), child);
            }
        }
        mkfs::Node::File { data, .. } => {
            let mut f = std::fs::File::create(path).expect("create file");
            f.write_all(data).expect("write");
        }
        mkfs::Node::Symlink { target, .. } => {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, path).expect("create symlink");
            #[cfg(not(unix))]
            {
                let _ = target;
                panic!("symlink materialization only supported on unix");
            }
        }
        mkfs::Node::Device { .. }
        | mkfs::Node::Special { .. }
        | mkfs::Node::ChunkedFile { .. }
        | mkfs::Node::CompressedFile(_) => {
            panic!("materialize_node: non-regular Node kinds aren't supported in oracle staging");
        }
    }
}

/// Run `mkfs.erofs <extra_args> out_path source_dir` IN THE GUEST.
/// Returns the exit status and captured streams for diagnosis; callers
/// decide what a non-zero status means.
pub fn run_mkfs_erofs(extra_args: &[&str], out_path: &Path, source_dir: &Path) -> RunResult {
    let mut call = oracle("mkfs.erofs");
    for a in extra_args {
        call = call.arg(a);
    }
    let out = call.arg(out_path).arg(source_dir).output();
    RunResult {
        status_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Build an EROFS image with `mkfs.erofs` from an in-memory Node tree
/// plus an args list. Returns the rendered image bytes, and a path to
/// the image on disk for tools (fsck/dump) that want one.
///
/// THE SOURCE TREE AND THE IMAGE LIVE INSIDE THIS REPOSITORY, under the
/// scratch directory, because that is the only tree the guest can see.
/// A `tempfile::tempdir()` under /tmp -- which is what this used to do
/// -- is a path `mkfs.erofs` in the guest cannot open.
pub fn build_with_mkfs_erofs(args: &[&str], tree: &mkfs::Node) -> ImageArtifact {
    let dir = ScratchDir::new("mkfs");
    let src = dir.join("src");
    let img = dir.join("out.img");
    materialize_tree(&src, tree);
    let result = run_mkfs_erofs(args, &img, &src);
    if result.status_code != Some(0) {
        panic!(
            "mkfs.erofs {:?} failed: code={:?}\nstderr: {}\nstdout: {}",
            args, result.status_code, result.stderr, result.stdout
        );
    }
    let bytes = std::fs::read(&img).expect("read built image");
    ImageArtifact {
        bytes,
        path: img,
        _guard: dir,
    }
}

/// Wraps the bytes of a built image alongside the scratch directory
/// keeping its on-disk twin alive for tools (fsck/dump) that want a
/// path. The bytes alone outlive the guard.
pub struct ImageArtifact {
    pub bytes: Vec<u8>,
    pub path: PathBuf,
    _guard: ScratchDir,
}

#[derive(Debug)]
pub struct RunResult {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Write `bytes` into the scratch directory and return the path, plus
/// the guard that deletes it. Inside the repository, so the guest can
/// read it.
pub fn stage_image(tag: &str, bytes: &[u8]) -> (PathBuf, ScratchDir) {
    let dir = ScratchDir::new(tag);
    let path = dir.join("image.img");
    std::fs::write(&path, bytes).expect("write image");
    (path, dir)
}

// ---- C ABI (capi) test helpers ----------------------------------------
//
// The `tests/capi_*.rs` suites drive `fs_erofs::capi` -- the exact code
// behind the exported C symbols. A staticlib doesn't re-export unmangled
// C symbols to integration tests, so (following the sibling fs-ext4
// suite) we call the `pub extern "C"` items through the rlib. The
// fixture tree lives here so every capi suite agrees on one image.

/// POSIX errno values the C ABI reports through `fs_erofs_last_errno()`.
/// Mirrors the hand-rolled constants in `src/capi.rs`, which avoids a
/// libc dependency just to name a handful of numbers.
pub mod errno {
    use std::os::raw::c_int;
    pub const ENOENT: c_int = 2;
    pub const EIO: c_int = 5;
    pub const ENOTDIR: c_int = 20;
    pub const EINVAL: c_int = 22;
    pub const ERANGE: c_int = 34;
}

/// Body of `/hello.txt` in the shared capi fixture. 12 bytes -- short
/// enough that mkfs stores it inline in the metadata area.
pub const HELLO: &[u8] = b"hello erofs\n";

/// Target of `/link` in the shared capi fixture.
pub const LINK_TARGET: &str = "hello.txt";

/// Target of `/longlink`. 200 bytes -- too long for the inline tail, so
/// the reader has to fetch it from a data block.
pub const LONG_LINK_TARGET_LEN: usize = 200;

/// Size of `/big.bin`: two whole 4 KiB blocks plus a 100-byte tail, so
/// reads that cross a block boundary and reads of a short final block
/// are both exercised.
pub const BIG_LEN: usize = 8192 + 100;

/// uid/gid/mtime stamped on `/owned.txt`. Any non-default `NodeMeta`
/// field promotes the inode to the 64-byte extended shape, which is the
/// only one carrying mtime -- so this fixture also covers the extended
/// inode path through `fill_attr`.
pub const OWNED_UID: u32 = 1000;
pub const OWNED_GID: u32 = 2000;
pub const OWNED_MTIME: u64 = 1_700_000_000;

/// Deterministic, non-repeating-per-block body for `/big.bin`. The 251
/// modulus is coprime with the 4096-byte block size, so no two blocks
/// share a byte pattern and an off-by-one-block read is detectable.
pub fn big_body() -> Vec<u8> {
    (0..BIG_LEN).map(|i| (i % 251) as u8).collect()
}

/// The 200-byte symlink target used by `/longlink`.
pub fn long_link_target() -> String {
    "a".repeat(LONG_LINK_TARGET_LEN)
}

/// Build a `mkfs::Node::Symlink` with the default symlink mode.
pub fn symlink(target: &str) -> mkfs::Node {
    mkfs::Node::Symlink {
        mode: mkfs::DEFAULT_SYMLINK_MODE,
        target: target.to_string(),
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// Build a `mkfs::Node::Device`. `mode` must carry S_IFCHR or S_IFBLK.
pub fn device(mode: u16, rdev: u32) -> mkfs::Node {
    mkfs::Node::Device {
        mode,
        rdev,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// Build a `mkfs::Node::Special`. `mode` must carry S_IFIFO or S_IFSOCK.
pub fn special(mode: u16) -> mkfs::Node {
    mkfs::Node::Special {
        mode,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// Build a `mkfs::Node::File` carrying explicit uid/gid/mtime.
pub fn file_with_meta(data: &[u8], meta: mkfs::NodeMeta) -> mkfs::Node {
    mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        meta,
        xattrs: Vec::new(),
    }
}

/// The tree every capi suite mounts. One node of each type the ABI can
/// describe, so `mode_to_abi` / `fill_attr` / `dir_entry_to_abi` are all
/// driven over real on-disk inodes rather than synthetic values.
///
/// ```text
/// /hello.txt   regular, 12 bytes, inline tail
/// /empty.bin   regular, 0 bytes
/// /big.bin     regular, 8292 bytes (multi-block)
/// /owned.txt   regular, extended inode (uid/gid/mtime set)
/// /link        symlink -> "hello.txt"        (inline target)
/// /longlink    symlink -> 200 x 'a'          (target in a data block)
/// /sub/nested.txt
/// /sub/deeper/leaf.txt
/// /chr /blk /fifo /sock
/// ```
pub fn capi_fixture_tree() -> mkfs::Node {
    use fs_erofs::inode::{S_IFBLK, S_IFCHR, S_IFIFO, S_IFSOCK};
    let big = big_body();
    let long = long_link_target();
    dir(vec![
        ("hello.txt", file(HELLO)),
        ("empty.bin", file(b"")),
        ("big.bin", file(&big)),
        (
            "owned.txt",
            file_with_meta(
                b"owned\n",
                mkfs::NodeMeta {
                    uid: OWNED_UID,
                    gid: OWNED_GID,
                    mtime: OWNED_MTIME,
                    mtime_nsec: 0,
                },
            ),
        ),
        ("link", symlink(LINK_TARGET)),
        ("longlink", symlink(&long)),
        (
            "sub",
            dir(vec![
                ("nested.txt", file(b"nested\n")),
                ("deeper", dir(vec![("leaf.txt", file(b"leaf\n"))])),
            ]),
        ),
        ("chr", device(S_IFCHR | 0o600, 0x0102)),
        ("blk", device(S_IFBLK | 0o660, 0x0802)),
        ("fifo", special(S_IFIFO | 0o644)),
        ("sock", special(S_IFSOCK | 0o755)),
    ])
}

/// Render [`capi_fixture_tree`] to image bytes with 4 KiB blocks.
pub fn capi_fixture_image() -> Vec<u8> {
    mkfs::build_image(capi_fixture_tree(), 12).expect("build capi fixture image")
}

/// An image parked in a temp dir, with its path pre-converted to the
/// `CString` that `fs_erofs_mount` wants. Dropping it removes the file,
/// so tests must hold it for as long as the mount lives.
pub struct TempImage {
    _dir: tempfile::TempDir,
    pub path: std::ffi::CString,
}

/// Write `bytes` to a fresh temp dir and return a handle to it.
pub fn temp_image(bytes: &[u8]) -> TempImage {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path().join("erofs.img");
    std::fs::write(&p, bytes).expect("write temp image");
    let path =
        std::ffi::CString::new(p.to_str().expect("utf-8 temp path")).expect("no NUL in path");
    TempImage { _dir: dir, path }
}

/// Read this thread's `fs_erofs_last_error()` as an owned `String`.
/// Never returns `<null>` in practice -- the thread-local is initialised
/// to an empty `CString` -- but the guard documents the ABI contract
/// that the pointer must always be dereferenceable.
pub fn capi_last_error() -> String {
    let p = fs_erofs::capi::fs_erofs_last_error();
    if p.is_null() {
        return "<null>".to_string();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

/// Decode a NUL-terminated `[c_char]` field (dirent name, volume name)
/// into a `Vec<u8>` of the bytes before the terminator. Panics if the
/// field has no NUL -- that itself is an ABI violation worth failing on.
pub fn cchar_field_to_bytes(field: &[std::os::raw::c_char]) -> Vec<u8> {
    let end = field
        .iter()
        .position(|&c| c == 0)
        .expect("C string field is not NUL-terminated");
    // `to_ne_bytes`, not `c as u8`: `c_char` is `u8` on aarch64-linux,
    // where the cast is a same-type cast clippy refuses (#89).
    field[..end].iter().map(|&c| c.to_ne_bytes()[0]).collect()
}

/// Recompute the superblock checksum after a test has edited superblock
/// bytes, so the edit is what the test exercises rather than the checksum
/// the reader verifies at open (#52). No-op when the image does not set
/// `EROFS_FEATURE_COMPAT_SB_CHKSUM`. Span and algorithm as
/// `Superblock::verify_checksum`: CRC32C from the superblock to the end of
/// its block, checksum field zeroed, no final XOR.
pub fn reseal_superblock(img: &mut [u8]) {
    const SB: usize = 1024;
    let compat = u32::from_le_bytes(img[SB + 0x08..SB + 0x0C].try_into().unwrap());
    if compat & fs_erofs::EROFS_FEATURE_COMPAT_SB_CHKSUM == 0 {
        return;
    }
    let block_size = 1usize << img[SB + 0x0C];
    let want_len = block_size - SB % block_size;
    let mut span = img[SB..SB + want_len].to_vec();
    span[4..8].fill(0);
    let crc = crc32c::crc32c(&span) ^ 0xFFFF_FFFF;
    img[SB + 0x04..SB + 0x08].copy_from_slice(&crc.to_le_bytes());
}
