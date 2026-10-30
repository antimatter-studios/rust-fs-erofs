//! The tail of an inode's data -- its last partial block -- goes in the
//! metadata area, right after the inode, whenever it fits in the block the
//! inode sits in (`FLAT_INLINE`), as `mkfs.erofs` lays it out. A small
//! directory, a symlink and a small file then take no data block at all,
//! and a large uncompressed file takes only its whole blocks.
//!
//! An Android tree has thousands of directories and symlinks, so a block
//! for each is the difference between an image that fits its partition and
//! one that does not. `oracle_writer.rs` has `fsck.erofs` and the kernel
//! read the same shapes.

mod common;

use common::{dir, file, open_image};
use fs_erofs::mkfs::{self, BuildOptions, Compression, Node, NodeMeta, DEFAULT_SYMLINK_MODE};
use fs_erofs::DataLayout;

fn symlink(target: &str) -> Node {
    Node::Symlink {
        mode: DEFAULT_SYMLINK_MODE,
        target: target.to_string(),
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[test]
fn a_symlink_target_is_inline() {
    let fs = open_image(
        mkfs::build_image(dir(vec![("sh", symlink("/system/bin/toybox"))]), 12).unwrap(),
    );
    let link = fs.lookup_path("/sh").unwrap();
    assert_eq!(link.format.layout, DataLayout::FlatInline);
    assert_eq!(
        fs.read_symlink_target(&link).unwrap(),
        b"/system/bin/toybox"
    );
}

#[test]
fn a_directory_tail_is_inline_and_every_entry_reads_back() {
    let names: Vec<String> = (0..600).map(|i| format!("entry-number-{i:04}")).collect();
    let big = dir(names
        .iter()
        .map(|n| (n.as_str(), file(n.as_bytes())))
        .collect());
    let fs = open_image(
        mkfs::build_image(
            dir(vec![("small", dir(vec![("a", file(b"a"))])), ("big", big)]),
            12,
        )
        .unwrap(),
    );
    let small = fs.lookup_path("/small").unwrap();
    assert_eq!(small.format.layout, DataLayout::FlatInline);
    assert!(small.size < 4096);
    assert_eq!(fs.read_dir(&small).unwrap().len(), 3);

    let big = fs.lookup_path("/big").unwrap();
    assert!(big.size > 4096, "600 entries take more than a block");
    let listed = fs.read_dir(&big).unwrap();
    assert_eq!(listed.len(), 602);
    for name in &names {
        let inode = fs.lookup_path(&format!("/big/{name}")).unwrap();
        let mut buf = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, name.as_bytes());
    }
}

#[test]
fn a_large_files_last_partial_block_is_inline() {
    let data = noise(10_000, 1);
    let fs = open_image(mkfs::build_image(dir(vec![("f", file(&data))]), 12).unwrap());
    let f = fs.lookup_path("/f").unwrap();
    assert_eq!(f.format.layout, DataLayout::FlatInline);
    let mut buf = vec![0u8; data.len()];
    fs.read_file(&f, 0, &mut buf).unwrap();
    assert_eq!(buf, data);
}

/// A file the compressor stores as it is has its tail inline too, and is
/// still read only once.
#[test]
fn a_file_stored_uncompressed_by_the_compressor_has_its_tail_inline() {
    let data = noise(50_001, 2);
    let options = BuildOptions {
        compression: Some(Compression::LZ4HC_12_32K),
        ..BuildOptions::default()
    };
    let fs =
        open_image(mkfs::build_image_with(dir(vec![("f", file(&data))]), 12, options).unwrap());
    let f = fs.lookup_path("/f").unwrap();
    assert_eq!(f.format.layout, DataLayout::FlatInline);
    let mut buf = vec![0u8; data.len()];
    fs.read_file(&f, 0, &mut buf).unwrap();
    assert_eq!(buf, data);
}

/// 200 directories, each holding a symlink and a 5000-byte file: one data
/// block per file, and every tail inline. A block for each directory,
/// symlink and tail would be over 800.
#[test]
fn a_tree_of_small_inodes_takes_a_block_per_files_whole_block_and_little_else() {
    let names: Vec<String> = (0..200).map(|i| format!("d{i:03}")).collect();
    let tree = dir(names
        .iter()
        .map(|n| {
            (
                n.as_str(),
                dir(vec![
                    ("link", symlink("../target")),
                    ("f", file(&noise(5000, 3))),
                ]),
            )
        })
        .collect());
    let img = mkfs::build_image(tree, 12).unwrap();
    let blocks = img.len() / 4096;
    assert!(blocks < 300, "{blocks} blocks");
    let fs = open_image(img);
    assert_eq!(
        fs.read_symlink_target(&fs.lookup_path("/d123/link").unwrap())
            .unwrap(),
        b"../target"
    );
}
