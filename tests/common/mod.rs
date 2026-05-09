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
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// Returns true if the `mkfs.erofs` binary is on `PATH` and runnable.
/// Tests that need it should branch on this and skip (or mark `#[ignore]`)
/// so CI without erofs-utils still passes.
pub fn mkfs_erofs_available() -> bool {
    Command::new("mkfs.erofs")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn fsck_erofs_available() -> bool {
    Command::new("fsck.erofs")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn dump_erofs_available() -> bool {
    Command::new("dump.erofs")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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

/// Spawn `mkfs.erofs <extra_args> out_path source_dir`. Returns the
/// exit status + captured stderr for diagnosis. Tests should panic on
/// non-zero; this fn just returns the result.
pub fn run_mkfs_erofs(extra_args: &[&str], out_path: &Path, source_dir: &Path) -> RunResult {
    let mut cmd = Command::new("mkfs.erofs");
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg(out_path);
    cmd.arg(source_dir);
    let out = cmd.output().expect("spawn mkfs.erofs");
    RunResult {
        status_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Build an EROFS image with `mkfs.erofs` from an in-memory Node tree
/// plus an args list. Returns the rendered image bytes (and tempdir
/// kept alive via `_guard`). Caller MUST hold the guard for the
/// duration of any path-based use; the bytes alone outlive the guard.
pub fn build_with_mkfs_erofs(args: &[&str], tree: &mkfs::Node) -> ImageArtifact {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    let img = dir.path().join("out.img");
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

/// Wraps the bytes of a built image alongside the tempdir keeping its
/// on-disk twin alive for tools (fsck/dump) that want a path.
pub struct ImageArtifact {
    pub bytes: Vec<u8>,
    pub path: PathBuf,
    _guard: tempfile::TempDir,
}

#[derive(Debug)]
pub struct RunResult {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}
