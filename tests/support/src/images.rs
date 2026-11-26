//! Looking at an image without holding it in memory.
//!
//! A test that proves an operation "changed nothing" by reading the whole
//! image before and after holds two copies of it, and libtest runs one test
//! per CPU, so the copies add up: on a sibling driver such tests ran a test
//! VM out of memory (#302). Everything here works through one or two 1 MiB
//! buffers, or refuses a file that is not small.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const CHUNK: usize = 1 << 20;
const BLOCK: usize = 4096;

/// The most [`read_small_file`] reads.
pub const SMALL_FILE: u64 = 1 << 20;

#[track_caller]
fn open(path: &Path) -> fs::File {
    fs::File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
}

#[track_caller]
fn length(file: &fs::File, path: &Path) -> u64 {
    file.metadata()
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .len()
}

/// Fill `buffer` from `file`, short only at its end.
#[track_caller]
fn fill(file: &mut fs::File, buffer: &mut [u8], path: &Path) -> usize {
    let mut have = 0;
    while have < buffer.len() {
        match file.read(&mut buffer[have..]) {
            Ok(0) => break,
            Ok(n) => have += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => panic!("read {}: {e}", path.display()),
        }
    }
    have
}

/// A whole file that is small by construction. One over [`SMALL_FILE`]
/// fails, so a disk image cannot arrive here unnoticed.
#[track_caller]
pub fn read_small_file(path: impl AsRef<Path>) -> Vec<u8> {
    let path = path.as_ref();
    let file = open(path);
    let len = length(&file, path);
    assert!(
        len <= SMALL_FILE,
        "{} is {len} bytes, over the {SMALL_FILE} a test reads whole: compare it \
         through images.rs instead",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(SMALL_FILE + 1)
        .read_to_end(&mut bytes)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        bytes.len() as u64 <= SMALL_FILE,
        "{} grew while it was read",
        path.display()
    );
    bytes
}

/// `len` bytes of an image from `offset`, at most [`SMALL_FILE`].
#[track_caller]
pub fn read_image_region(path: impl AsRef<Path>, offset: u64, len: usize) -> Vec<u8> {
    let path = path.as_ref();
    assert!(len as u64 <= SMALL_FILE, "a region of {len} bytes");
    let mut file = open(path);
    file.seek(SeekFrom::Start(offset))
        .unwrap_or_else(|e| panic!("seek {}: {e}", path.display()));
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)
        .unwrap_or_else(|e| panic!("read {len} at {offset} of {}: {e}", path.display()));
    bytes
}

/// Overwrite one region of an image, leaving the rest and its holes alone.
#[track_caller]
pub fn write_image_region(path: impl AsRef<Path>, offset: u64, bytes: &[u8]) {
    let path = path.as_ref();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.write_all(bytes))
        .unwrap_or_else(|e| panic!("write at {offset} of {}: {e}", path.display()));
}

/// What writing the file at `path` answered, or the test's failure naming it.
#[track_caller]
fn written<T>(result: std::io::Result<T>, path: &Path) -> T {
    result.unwrap_or_else(|e| panic!("write {}: {e}", path.display()))
}

/// Copy `from` into a new file at `to`, `offset` bytes in, leaving a hole
/// wherever the bytes before it or `from`'s own blocks are zeros.
#[track_caller]
pub fn copy_sparse_at(from: impl AsRef<Path>, to: impl AsRef<Path>, offset: u64) {
    let (from, to) = (from.as_ref(), to.as_ref());
    let mut source = open(from);
    let len = length(&source, from);
    let mut target = written(fs::File::create(to), to);
    written(target.set_len(offset + len), to);
    let mut buffer = vec![0u8; CHUNK];
    let mut at = offset;
    loop {
        let read = fill(&mut source, &mut buffer, from);
        if read == 0 {
            return;
        }
        for (index, block) in buffer[..read].chunks(BLOCK).enumerate() {
            if block.iter().any(|&byte| byte != 0) {
                let to_block = SeekFrom::Start(at + (index * BLOCK) as u64);
                written(target.seek(to_block), to);
                written(target.write_all(block), to);
            }
        }
        at += read as u64;
    }
}

/// A sparse copy of an image at one point in a test, removed when dropped.
pub struct ImageSnapshot(PathBuf);

impl Drop for ImageSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Keep an image as it is now, on disk.
#[track_caller]
pub fn snapshot_image(image: impl AsRef<Path>) -> ImageSnapshot {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = crate::temp_dir().join(format!(
        "image-snapshot-{}-{}.img",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    copy_sparse_at(image, &path, 0);
    ImageSnapshot(path)
}

/// Where two files first differ, or `None` when they are the same bytes.
#[track_caller]
pub fn first_difference(left: impl AsRef<Path>, right: impl AsRef<Path>) -> Option<String> {
    let (left, right) = (left.as_ref(), right.as_ref());
    let (mut a, mut b) = (open(left), open(right));
    let (a_len, b_len) = (length(&a, left), length(&b, right));
    if a_len != b_len {
        return Some(format!("lengths differ: {a_len} and {b_len} bytes"));
    }
    let (mut x, mut y) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut offset = 0u64;
    loop {
        let n = fill(&mut a, &mut x, left);
        let m = fill(&mut b, &mut y, right);
        if let Some(i) = x[..n].iter().zip(&y[..m]).position(|(p, q)| p != q) {
            return Some(format!(
                "first differing byte at {:#x}: {:#04x} and {:#04x}",
                offset + i as u64,
                x[i],
                y[i]
            ));
        }
        if n != m {
            return Some(format!("one file ended early, after {offset} bytes"));
        }
        if n == 0 {
            return None;
        }
        offset += n as u64;
    }
}

/// Two files hold the same bytes.
#[track_caller]
pub fn assert_images_equal(left: impl AsRef<Path>, right: impl AsRef<Path>, context: &str) {
    if let Some(difference) = first_difference(left, right) {
        panic!("{context}: {difference}");
    }
}

/// An image is still what it was when `before` was taken.
#[track_caller]
pub fn assert_image_unchanged(image: impl AsRef<Path>, before: &ImageSnapshot, context: &str) {
    assert_images_equal(image, &before.0, context);
}

/// A file holds exactly `expected`.
#[track_caller]
pub fn assert_file_bytes(path: impl AsRef<Path>, expected: &[u8], context: &str) {
    let path = path.as_ref();
    let mut file = open(path);
    let len = length(&file, path);
    assert_eq!(
        len,
        expected.len() as u64,
        "{context}: the length of {}",
        path.display()
    );
    let mut buffer = vec![0u8; CHUNK];
    for (part, want) in expected.chunks(CHUNK).enumerate() {
        let n = fill(&mut file, &mut buffer[..want.len()], path);
        assert_eq!(n, want.len(), "{context}: {} ended early", path.display());
        if let Some(i) = buffer[..n].iter().zip(want).position(|(a, b)| a != b) {
            panic!(
                "{context}: first differing byte at {:#x}: {:#04x}, expected {:#04x}",
                part * CHUNK + i,
                buffer[i],
                want[i]
            );
        }
    }
}

/// The SHA-256 of a file, in the hex `sha256sum` prints.
#[track_caller]
pub fn sha256_file(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let mut file = open(path);
    let mut hash = crate::kernel::Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let n = fill(&mut file, &mut buffer, path);
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    hash.hex()
}
