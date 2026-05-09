//! Stress + property-style + negative tests.
//!
//! These exercise corners of the reader that single-shape unit tests
//! miss: random-tree generation (deterministic seed), unicode + max-
//! length names, and malformed-image refusal (panics MUST NOT escape
//! parsing -- we wrap each parse in `catch_unwind`).

mod common;

use common::{dir, file, open_image, MemDev};
use fs_erofs::{mkfs, Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::Arc;

// ---- deterministic PRNG -----------------------------------------------

/// Tiny xorshift64 PRNG -- deterministic, no `rand` crate dep.
struct Xs64(u64);
impl Xs64 {
    fn new(seed: u64) -> Self {
        Xs64(if seed == 0 {
            0xdead_beef_cafe_babe
        } else {
            seed
        })
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo)
    }
}

// ---- random-tree generation -------------------------------------------

/// Generate a random tree of approximately `target_nodes` nodes. Each
/// node has a random ascii name; files get random byte payloads of
/// random size in `0..max_file_size`. Small dirs (<=200 children) keep
/// us under the writer's single-block-dir cap.
fn random_tree(seed: u64, target_nodes: usize, max_file_size: usize) -> mkfs::Node {
    let mut rng = Xs64::new(seed);
    let mut count = 0usize;
    let mut counter = 0u32;
    fn name(rng: &mut Xs64, counter: &mut u32) -> String {
        // Mix a counter into the name to guarantee uniqueness within a
        // dir even if RNG happens to collide.
        let len = rng.range(3, 14) as usize;
        let mut s = String::with_capacity(len + 6);
        for _ in 0..len {
            let c = (b'a' + (rng.next() % 26) as u8) as char;
            s.push(c);
        }
        s.push('_');
        s.push_str(&counter.to_string());
        *counter += 1;
        s
    }
    fn build(
        rng: &mut Xs64,
        count: &mut usize,
        counter: &mut u32,
        target: usize,
        max_size: usize,
        depth: usize,
    ) -> mkfs::Node {
        let mut entries = BTreeMap::new();
        let n_children = if depth >= 4 {
            0
        } else {
            (rng.range(2, 8) as usize).min(target.saturating_sub(*count))
        };
        for _ in 0..n_children {
            *count += 1;
            // 25% chance of subdir, 75% file.
            let make_dir = rng.next().is_multiple_of(4) && depth < 3;
            let nm = name(rng, counter);
            let child = if make_dir {
                build(rng, count, counter, target, max_size, depth + 1)
            } else {
                let sz = rng.range(0, max_size as u64 + 1) as usize;
                let mut data = vec![0u8; sz];
                for b in &mut data {
                    *b = rng.next() as u8;
                }
                mkfs::Node::File {
                    mode: mkfs::DEFAULT_FILE_MODE,
                    data,
                    meta: mkfs::NodeMeta::default(),
                    xattrs: Vec::new(),
                }
            };
            entries.insert(nm, child);
        }
        mkfs::Node::Dir {
            mode: mkfs::DEFAULT_DIR_MODE,
            entries,
            meta: mkfs::NodeMeta::default(),
            xattrs: Vec::new(),
        }
    }
    build(
        &mut rng,
        &mut count,
        &mut counter,
        target_nodes,
        max_file_size,
        0,
    )
}

/// Walk an `mkfs::Node` tree and produce `(absolute_path, file_data)`
/// for every file leaf.
fn collect_files(node: &mkfs::Node, prefix: String, out: &mut Vec<(String, Vec<u8>)>) {
    match node {
        mkfs::Node::Dir { entries, .. } => {
            for (name, child) in entries {
                let p = format!("{prefix}/{name}");
                collect_files(child, p, out);
            }
        }
        mkfs::Node::File { data, .. } => {
            out.push((prefix, data.clone()));
        }
        mkfs::Node::Symlink { .. }
        | mkfs::Node::Device { .. }
        | mkfs::Node::Special { .. }
        | mkfs::Node::ChunkedFile { .. }
        | mkfs::Node::CompressedFile(_) => {}
    }
}

#[test]
fn random_tree_round_trips() {
    let tree = random_tree(0xdead_beef, 100, 8192);
    let mut expected = Vec::new();
    collect_files(&tree, String::new(), &mut expected);

    let img = mkfs::build_image(tree, 12).unwrap();
    let fs = open_image(img);

    for (path, want) in &expected {
        let inode = fs
            .lookup_path(path)
            .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
        let mut buf = vec![0u8; want.len()];
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
        assert_eq!(buf, *want, "{path} content mismatch");
    }
}

// ---- name edge cases ---------------------------------------------------

#[test]
fn unicode_filename_round_trips() {
    let img =
        mkfs::build_image(dir(vec![("héllo-世界.txt", file(b"unicode-name\n"))]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/héllo-世界.txt").unwrap();
    let mut buf = vec![0u8; 13];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"unicode-name\n");
}

#[test]
fn filename_with_spaces_round_trips() {
    let img = mkfs::build_image(dir(vec![("file with spaces.txt", file(b"yes\n"))]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/file with spaces.txt").unwrap();
    let mut buf = vec![0u8; 4];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"yes\n");
}

#[test]
fn max_length_filename_round_trips() {
    // 255 bytes is the Linux NAME_MAX. EROFS doesn't enforce this -- the
    // limit is "fits in the dir block alongside the dirent header" --
    // but it's the realistic ceiling consumers will hit.
    let long_name: String = "x".repeat(255);
    let img = mkfs::build_image(dir(vec![(long_name.as_str(), file(b"long-name\n"))]), 12).unwrap();
    let fs = open_image(img);
    let path = format!("/{long_name}");
    let inode = fs.lookup_path(&path).unwrap();
    let mut buf = vec![0u8; 10];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"long-name\n");
}

// ---- malformed-image negative tests -----------------------------------
//
// The reader must REFUSE corrupt images with an `Error::*` rather than
// panicking. We wrap each open in `catch_unwind` and require both:
// (a) no panic escapes, and (b) the result is `Err(_)`.

fn build_simple_image() -> Vec<u8> {
    mkfs::build_image(
        dir(vec![
            ("a.txt", file(b"hello\n")),
            ("b.bin", file(&[0xCDu8; 100])),
            ("sub", dir(vec![("inner.txt", file(b"inner\n"))])),
        ]),
        12,
    )
    .unwrap()
}

/// Run `Filesystem::open` inside a `catch_unwind` and return either:
/// - `Ok(Some(err))` if it returned `Err(_)` (the desired path),
/// - `Ok(None)` if it surprisingly returned `Ok(_)`,
/// - `Err(panic_payload)` if a panic escaped (the bug we're hunting).
fn open_classify(bytes: Vec<u8>) -> std::thread::Result<Option<Error>> {
    std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(bytes));
        Filesystem::open(dev).err()
    })
}

#[test]
fn flipped_magic_is_rejected_no_panic() {
    let mut img = build_simple_image();
    // SB magic at byte 1024.
    img[1024] ^= 0xFF;
    let opt = open_classify(img).expect("must not panic");
    let err = opt.expect("open should error");
    assert!(
        matches!(err, Error::NotErofs | Error::BadSuperblock(_)),
        "expected NotErofs/BadSuperblock, got {err:?}",
    );
}

#[test]
fn flipped_blkszbits_is_rejected_no_panic() {
    let mut img = build_simple_image();
    // blkszbits at byte 1024 + 0x0C = 1036.
    img[1024 + 0x0C] = 99;
    let opt = open_classify(img).expect("must not panic");
    let err = opt.expect("open should error");
    assert!(
        matches!(err, Error::BadSuperblock(_)),
        "expected BadSuperblock, got {err:?}",
    );
}

#[test]
fn corrupt_root_nid_is_handled_no_panic() {
    let mut img = build_simple_image();
    // Set root_nid to a wildly out-of-range value.
    img[1024 + 0x0E..1024 + 0x10].copy_from_slice(&0xFFFFu16.to_le_bytes());
    let res: std::thread::Result<fs_erofs::Result<()>> = std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(img));
        let fs = Filesystem::open(dev)?;
        fs.root_inode().map(|_| ())
    });
    let result = res.expect("must not panic");
    assert!(result.is_err(), "reading huge root_nid should error");
}

#[test]
fn corrupt_inode_format_is_handled_no_panic() {
    let mut img = build_simple_image();
    // The first inode (root, NID 0) starts at meta_blkaddr * blocksize
    // = 1 * 4096 = 4096. Stomp i_format with a bogus layout (bits
    // 1..=3 = 7, an undefined layout).
    let bogus_format: u16 = 0b1110; // version=0, layout=7 (invalid), flags=0
    img[4096..4098].copy_from_slice(&bogus_format.to_le_bytes());
    let res: std::thread::Result<fs_erofs::Result<()>> = std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(img));
        let fs = Filesystem::open(dev)?;
        fs.root_inode().map(|_| ())
    });
    let result = res.expect("must not panic");
    assert!(
        matches!(result, Err(Error::BadInode(_))),
        "expected BadInode, got {result:?}",
    );
}

#[test]
fn corrupt_dirent_nameoff_is_handled_no_panic() {
    let mut img = build_simple_image();
    // The root dir's data block sits right after the inode area. With
    // a small tree that fits in one inode block, the dir area starts
    // at block 2 (byte 8192). Stomp the first dirent's nameoff to a
    // value not divisible by EROFS_DIRENT_SIZE (=12).
    let dir_block_off = 2 * 4096;
    img[dir_block_off + 8..dir_block_off + 10].copy_from_slice(&5u16.to_le_bytes());
    let res: std::thread::Result<fs_erofs::Result<()>> = std::panic::catch_unwind(move || {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(MemDev::new(img));
        let fs = Filesystem::open(dev)?;
        let root = fs.root_inode()?;
        fs.read_dir(&root).map(|_| ())
    });
    let result = res.expect("must not panic");
    assert!(
        matches!(result, Err(Error::BadDirent(_))),
        "expected BadDirent, got {result:?}",
    );
}

#[test]
fn truncated_image_is_handled_no_panic() {
    // Buffer too short to hold a superblock.
    let img = vec![0u8; 100];
    let opt = open_classify(img).expect("must not panic");
    assert!(opt.is_some(), "expected error for tiny image");
}

#[test]
fn all_zeros_image_is_rejected_no_panic() {
    let img = vec![0u8; 8192];
    let opt = open_classify(img).expect("must not panic");
    let err = opt.expect("open should error");
    assert!(
        matches!(err, Error::NotErofs | Error::BadSuperblock(_)),
        "expected NotErofs/BadSuperblock for zeros, got {err:?}",
    );
}
