//! The writer streams: each file's bytes come from a reader the caller
//! supplies, opened once when the writer reaches it, and the image goes to
//! a `Write + Seek` sink. File data is written first, as it is read; the
//! metadata area and directories follow it, once every inode's shape is
//! known, and the superblock is written last.
//!
//! What these hold: a streamed file builds the same image as the same
//! bytes in memory, every content is opened and read exactly once, content
//! of the wrong length is refused, and the metadata area sits after the
//! data. `streaming_memory.rs` holds the bound on memory, in a binary of
//! its own so no other test's allocations reach its counter.

mod common;

use common::{dir, file, open_image};
use fs_erofs::mkfs::{self, BuildOptions, Content, Node, NodeMeta, DEFAULT_FILE_MODE};
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn streamed(content: Content) -> Node {
    Node::StreamedFile {
        mode: DEFAULT_FILE_MODE,
        content,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// One of every size the layout treats differently: empty, inline, one
/// block exactly, and many blocks with a partial tail.
fn sizes() -> [(&'static str, usize); 4] {
    [
        ("empty", 0),
        ("inline", 700),
        ("block", 4096),
        ("big", 300_001),
    ]
}

#[test]
fn a_streamed_file_builds_the_same_image_as_the_same_bytes_in_memory() {
    let in_memory = dir(sizes()
        .iter()
        .map(|(name, len)| (*name, file(&pattern(*len, len.to_le_bytes()[0]))))
        .collect());
    let from_readers = dir(sizes()
        .iter()
        .map(|(name, len)| {
            (
                *name,
                streamed(Content::from_bytes(pattern(*len, len.to_le_bytes()[0]))),
            )
        })
        .collect());
    assert_eq!(
        mkfs::build_image(in_memory, 12).unwrap(),
        mkfs::build_image(from_readers, 12).unwrap()
    );
}

/// A reader that counts what is asked of it.
struct Counted {
    inner: Cursor<Vec<u8>>,
    read: Arc<AtomicUsize>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

#[test]
fn every_content_is_opened_once_and_read_once_and_reads_back() {
    let opens = Arc::new(AtomicUsize::new(0));
    let read = Arc::new(AtomicUsize::new(0));
    let entries = sizes().map(|(name, len)| {
        let (opens, read) = (opens.clone(), read.clone());
        let bytes = pattern(len, 7);
        let node = streamed(Content::new(len as u64, move || {
            opens.fetch_add(1, Ordering::SeqCst);
            Ok(Counted {
                inner: Cursor::new(bytes),
                read,
            })
        }));
        (name, node)
    });
    let img = mkfs::build_image(dir(entries.into_iter().collect()), 12).unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), sizes().len());
    let total: usize = sizes().iter().map(|(_, len)| len).sum();
    assert_eq!(read.load(Ordering::SeqCst), total, "each byte read once");

    let fs = open_image(img);
    for (name, len) in sizes() {
        let inode = fs.lookup_path(&format!("/{name}")).unwrap();
        assert_eq!(inode.size, len as u64, "{name}");
        let mut buf = vec![0u8; len];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, pattern(len, 7), "{name}");
    }
}

#[test]
fn content_shorter_or_longer_than_its_size_is_refused() {
    for (label, declared, actual) in [
        ("short inline", 100u64, 99usize),
        ("long inline", 100, 101),
        ("short", 50_000, 49_999),
        ("long", 50_000, 50_001),
    ] {
        let node = streamed(Content::new(declared, move || {
            Ok(Cursor::new(vec![1u8; actual]))
        }));
        assert!(
            mkfs::build_image(dir(vec![("f", node)]), 12).is_err(),
            "{label}: content of {actual} bytes declared as {declared}"
        );
    }
}

#[test]
fn build_image_to_writes_what_build_image_with_returns() {
    let tree = || {
        dir(vec![
            ("a", file(&pattern(9000, 1))),
            (
                "sub",
                dir(vec![("b", streamed(Content::from_bytes(pattern(40, 2))))]),
            ),
        ])
    };
    let options = || BuildOptions {
        build_time: 1_230_768_000,
        mtime_is_build_time: true,
        ..BuildOptions::default()
    };
    let mut sink = Cursor::new(Vec::new());
    let len = mkfs::build_image_to(tree(), 12, options(), &mut sink).unwrap();
    let img = sink.into_inner();
    assert_eq!(len, img.len() as u64);
    assert_eq!(img, mkfs::build_image_with(tree(), 12, options()).unwrap());
}

/// The layout that lets one pass write the file data: data first, from
/// the block after the superblock, then the metadata area, whose position
/// the superblock records.
#[test]
fn the_metadata_area_follows_the_file_data() {
    let img = mkfs::build_image(dir(vec![("big", file(&pattern(300_001, 3)))]), 12).unwrap();
    let fs = open_image(img);
    let big = fs.lookup_path("/big").unwrap();
    // Its whole blocks; the last partial one is inline, after the inode.
    let data_blocks = (big.size / 4096) as u32;
    assert_eq!(
        big.raw_u, 1,
        "the data starts at the first block after the superblock"
    );
    assert!(
        fs.superblock().meta_blkaddr >= big.raw_u + data_blocks,
        "metadata area at block {} overlaps or precedes the data, blocks {}..{}",
        fs.superblock().meta_blkaddr,
        big.raw_u,
        big.raw_u + data_blocks
    );
}
