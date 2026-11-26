//! A file's pclusters taken from one image into another as they are:
//! `Transplant::of` describes them, and `Node::TransplantedFile` writes
//! them under an index of the new image's own, with nothing decompressed
//! and nothing compressed.
//!
//! These read every file back through this crate's reader, from an image
//! built of another's pclusters, and hold that it is the file that went
//! into the first. Both images here are this crate's own;
//! `transplant_oracle.rs` takes the pclusters `mkfs.erofs` wrote, and has
//! `fsck.erofs` and the kernel read what was built of them.

mod common;

use common::{dir, file, open_image};
use fs_erofs::mkfs::{
    self, BuildOptions, Compression, Content, Node, NodeMeta, Pcluster, Transplant,
    DEFAULT_FILE_MODE,
};
use fs_erofs::{DataLayout, Filesystem};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn compressing(max_pcluster_bytes: u32) -> BuildOptions {
    BuildOptions {
        compression: Some(Compression {
            lz4hc_level: 12,
            max_pcluster_bytes,
        }),
        ..BuildOptions::default()
    }
}

/// Text that compresses about as well as a configuration file does.
fn text(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "system", "vendor", "service", "class", "user", "group", "oneshot", "disabled", "socket",
        "property", "trigger", "start", "write", "chmod", "0755", "/data",
    ];
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(WORDS[(state % 16) as usize].as_bytes());
        out.push(if state.is_multiple_of(7) { b'\n' } else { b' ' });
    }
    out.truncate(len);
    out
}

/// Bytes that do not compress.
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

/// Files of every shape the compressor writes: all compressed, with a
/// partial last block and without; stored blocks between compressed
/// pclusters; more than one 4 MiB segment; a pcluster of zeros that holds
/// far more than it fills; and three it stores uncompressed, two of whole
/// blocks with a tail inline after the inode and one of whole blocks alone.
fn files() -> Vec<(&'static str, Vec<u8>)> {
    let mut mixed = text(70_001, 1);
    mixed.extend(noise(50_000, 2));
    mixed.extend(text(90_000, 3));
    mixed.extend(noise(4_097, 4));
    vec![
        ("text", text(300_001, 5)),
        ("blocks", text(40 * 4096, 6)),
        ("mixed", mixed),
        ("segments", text((9 << 20) + 123, 7)),
        ("zeros", vec![0u8; 3 << 20]),
        ("two", text(4097, 8)),
        ("noise", noise(100_000, 9)),
        ("noise-blocks", noise(40 * 4096, 12)),
        ("inline", text(700, 10)),
    ]
}

/// The names of [`files`] the compressor stores uncompressed, and the
/// layout it gives each.
const FLAT: [(&str, DataLayout); 3] = [
    ("noise", DataLayout::FlatInline),
    ("noise-blocks", DataLayout::FlatPlain),
    ("inline", DataLayout::FlatInline),
];

fn source(options: BuildOptions) -> Vec<u8> {
    let tree = dir(files()
        .iter()
        .map(|(name, data)| (*name, file(data)))
        .collect());
    mkfs::build_image_with(tree, 12, options).unwrap()
}

fn read_back(fs: &Filesystem, path: &str) -> Vec<u8> {
    let inode = fs.lookup_path(path).unwrap();
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    buf
}

fn transplanted(data: Transplant<'_>) -> Node<'_> {
    Node::TransplantedFile {
        mode: DEFAULT_FILE_MODE,
        data,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// A file of these bytes, in a tree that may borrow.
fn plain<'a>(data: &[u8]) -> Node<'a> {
    Node::File {
        mode: DEFAULT_FILE_MODE,
        data: data.to_vec(),
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// A root directory holding `f` alone.
fn alone(f: Node<'_>) -> Node<'_> {
    Node::Dir {
        mode: mkfs::DEFAULT_DIR_MODE,
        entries: [("f".to_string(), f)].into(),
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// Every file of `from`, transplanted where it can be and read out where
/// it cannot, with a file of its own between them.
fn rebuilt(from: &Filesystem, options: BuildOptions) -> Vec<u8> {
    let mut entries = std::collections::BTreeMap::new();
    for (name, _) in files() {
        let inode = from.lookup_path(&format!("/{name}")).unwrap();
        let node = match Transplant::of(from, &inode).unwrap() {
            Some(data) => transplanted(data),
            None => plain(&read_back(from, &format!("/{name}"))),
        };
        entries.insert(name.to_string(), node);
    }
    entries.insert("new".into(), plain(&text(123_456, 11)));
    let tree = Node::Dir {
        mode: mkfs::DEFAULT_DIR_MODE,
        entries,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    };
    mkfs::build_image_with(tree, 12, options).unwrap()
}

fn assert_every_file_reads_back(img: Vec<u8>) {
    let fs = open_image(img);
    for (name, data) in files() {
        assert!(
            read_back(&fs, &format!("/{name}")) == data,
            "{name} reads back different"
        );
    }
    assert!(read_back(&fs, "/new") == text(123_456, 11));
}

#[test]
fn a_compressed_file_and_a_flat_one_can_be_transplanted() {
    let fs = open_image(source(compressing(32 << 10)));
    for (name, data) in files() {
        let inode = fs.lookup_path(&format!("/{name}")).unwrap();
        let transplant = Transplant::of(&fs, &inode)
            .unwrap()
            .unwrap_or_else(|| panic!("{name}"));
        assert_eq!(transplant.size(), data.len() as u64);
        assert_eq!(transplant.blkszbits(), 12);
        assert_eq!(transplant.pclusters()[0].offset, 0);
        if let Some((_, layout)) = FLAT.iter().find(|(flat, _)| *flat == name) {
            assert_eq!(inode.format.layout, *layout, "{name}");
            // A block of the file, stored as it is, for each block of it.
            let blocks = (data.len() as u64).div_ceil(4096);
            let stored: Vec<Pcluster> = (0..blocks)
                .map(|at| pcluster(at * 4096, 1, false))
                .collect();
            assert_eq!(transplant.pclusters(), stored, "{name}");
        }
    }
}

#[test]
fn a_transplanted_flat_file_is_flat_in_the_image_that_takes_it() {
    let from = open_image(source(compressing(32 << 10)));
    let fs = open_image(rebuilt(&from, compressing(32 << 10)));
    for (name, layout) in FLAT {
        let inode = fs.lookup_path(&format!("/{name}")).unwrap();
        assert_eq!(inode.format.layout, layout, "{name}");
    }
}

#[test]
fn transplanted_files_read_back_from_an_image_that_compresses() {
    for max in [4 << 10, 32 << 10, 1 << 20] {
        let from = open_image(source(compressing(max)));
        assert_every_file_reads_back(rebuilt(&from, compressing(32 << 10)));
    }
}

#[test]
fn transplanted_files_read_back_from_an_image_that_compresses_nothing() {
    let from = open_image(source(compressing(32 << 10)));
    let img = rebuilt(&from, BuildOptions::default());
    let fs = open_image(img.clone());
    // Its own file is stored as it is; the transplanted one is compressed.
    let layout = |path| fs.lookup_path(path).unwrap().format.layout;
    assert_eq!(layout("/new"), DataLayout::FlatInline);
    assert_eq!(layout("/text"), DataLayout::CompressionLegacy);
    assert_every_file_reads_back(img);
}

#[test]
fn a_transplanted_image_can_be_transplanted_again() {
    let first = open_image(source(compressing(32 << 10)));
    let second = open_image(rebuilt(&first, compressing(32 << 10)));
    assert_every_file_reads_back(rebuilt(&second, compressing(32 << 10)));
}

#[test]
fn a_transplanted_file_takes_the_blocks_its_source_gave_it() {
    let from = open_image(source(compressing(32 << 10)));
    let blocks = |fs: &Filesystem| fs.lookup_path("/text").unwrap().raw_u;
    let to = open_image(rebuilt(&from, compressing(32 << 10)));
    assert_eq!(blocks(&from), blocks(&to));
}

#[test]
fn the_same_transplants_build_the_same_bytes_on_any_number_of_threads() {
    let from = open_image(source(compressing(32 << 10)));
    let with = |threads| BuildOptions {
        threads,
        ..compressing(32 << 10)
    };
    assert!(rebuilt(&from, with(1)) == rebuilt(&from, with(4)));
}

#[test]
fn the_blocks_are_opened_once_and_read_once() {
    let from = open_image(source(compressing(32 << 10)));
    let inode = from.lookup_path("/text").unwrap();
    let transplant = Transplant::of(&from, &inode).unwrap().unwrap();
    let pclusters = transplant.pclusters().to_vec();
    let total: u64 = pclusters.iter().map(|p| p.blocks as u64).sum();
    // Alone in an image, the file's blocks are the first after the
    // superblock's.
    let img = mkfs::build_image(alone(transplanted(transplant)), 12).unwrap();
    let fs = open_image(img.clone());
    let blocks = img[4096..4096 + (total * 4096) as usize].to_vec();

    struct Counted(Cursor<Vec<u8>>, Arc<AtomicUsize>);
    impl std::io::Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }
    let (read, opens) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (r, o, bytes) = (read.clone(), opens.clone(), blocks.clone());
    let again = Transplant::new(
        12,
        inode.size,
        pclusters,
        Content::new(blocks.len() as u64, move || {
            o.fetch_add(1, Ordering::SeqCst);
            Ok(Counted(Cursor::new(bytes), r))
        }),
    )
    .unwrap();
    let rebuilt = mkfs::build_image(alone(transplanted(again)), 12).unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(read.load(Ordering::SeqCst), blocks.len());
    assert!(rebuilt == img);
    assert!(read_back(&fs, "/f") == text(300_001, 5));
}

/// Whether pclusters of 4 KiB blocks, over `blocks` blocks of zeros, are
/// taken as a file of `size` bytes.
fn taken(size: u64, pclusters: Vec<Pcluster>, blocks: usize) -> bool {
    Transplant::new(
        12,
        size,
        pclusters,
        Content::from_bytes(vec![0u8; blocks * 4096]),
    )
    .is_ok()
}

fn pcluster(offset: u64, blocks: u32, compressed: bool) -> Pcluster {
    Pcluster {
        offset,
        blocks,
        compressed,
    }
}

#[test]
fn pclusters_no_file_is_made_of_are_refused() {
    let two = || vec![pcluster(0, 1, true), pcluster(9000, 1, true)];
    assert!(taken(20_000, two(), 2));
    // More blocks than the pclusters fill, and fewer.
    assert!(!taken(20_000, two(), 3));
    assert!(!taken(20_000, two(), 1));
    // None, not from the start of the file, out of order, or past its end.
    assert!(!taken(20_000, Vec::new(), 0));
    assert!(!taken(20_000, vec![pcluster(100, 1, true)], 1));
    assert!(!taken(
        20_000,
        vec![pcluster(0, 1, true), pcluster(0, 1, true)],
        2
    ));
    assert!(!taken(
        20_000,
        vec![pcluster(0, 1, true), pcluster(20_000, 1, true)],
        2
    ));
    // Two that start in one block of the file.
    assert!(!taken(
        20_000,
        vec![pcluster(0, 1, true), pcluster(100, 1, true)],
        2
    ));
    // No blocks, more than 1 MiB of them, and more than a block stored.
    assert!(!taken(20_000, vec![pcluster(0, 0, true)], 0));
    assert!(taken(20_000, vec![pcluster(0, 256, true)], 256));
    assert!(!taken(20_000, vec![pcluster(0, 257, true)], 257));
    assert!(!taken(20_000, vec![pcluster(0, 1, false)], 1));
    assert!(!taken(20_000, vec![pcluster(0, 2, false)], 2));
    assert!(taken(4_000, vec![pcluster(0, 1, false)], 1));
    // A block size no image has.
    let blocks = || Content::from_bytes(vec![0u8; 4096]);
    assert!(Transplant::new(8, 20_000, vec![pcluster(0, 1, true)], blocks()).is_err());
}

#[test]
fn an_image_of_another_block_size_refuses_the_pclusters() {
    let from = open_image(source(compressing(32 << 10)));
    let inode = from.lookup_path("/text").unwrap();
    let take = || transplanted(Transplant::of(&from, &inode).unwrap().unwrap());
    assert!(mkfs::build_image(alone(take()), 12).is_ok());
    assert!(mkfs::build_image(alone(take()), 13).is_err());
}

#[test]
fn blocks_that_end_early_or_go_on_are_refused() {
    let build = |actual: usize| {
        let data = Transplant::new(
            12,
            20_000,
            vec![pcluster(0, 1, true), pcluster(9000, 1, true)],
            Content::new(8192, move || Ok(Cursor::new(vec![0u8; actual]))),
        )
        .unwrap();
        mkfs::build_image(alone(transplanted(data)), 12)
    };
    assert!(build(8192).is_ok());
    assert!(build(8191).is_err());
    assert!(build(8193).is_err());
}
