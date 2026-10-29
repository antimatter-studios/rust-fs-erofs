//! Regular files compressed as `mkfs.erofs -zlz4hc,12 -C 32768` compresses
//! them: LZ4HC, as much of the file in each physical cluster as fits in
//! 32 KiB, a block stored as it is where compressing it saves nothing, and
//! a file where nothing compresses stored uncompressed.
//!
//! These read every file back through this crate's reader;
//! `oracle_writer.rs` has `fsck.erofs` and the kernel read the same shapes.

mod common;

use common::{dir, file, open_image};
use fs_core::BlockRead;
use fs_erofs::mkfs::{self, BuildOptions, Compression, Content, Node, NodeMeta, DEFAULT_FILE_MODE};
use fs_erofs::{DataLayout, Filesystem, ZMap};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn options() -> BuildOptions {
    BuildOptions {
        compression: Some(Compression::LZ4HC_12_32K),
        ..BuildOptions::default()
    }
}

/// Text that compresses about as well as a configuration file or a
/// script does: words from a small vocabulary, in an order that repeats
/// only loosely.
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

/// Bytes that do not compress: an APK's stored entries, or a JPEG.
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

fn read_back(fs: &Filesystem, path: &str) -> Vec<u8> {
    let inode = fs.lookup_path(path).unwrap();
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    buf
}

/// Every pcluster's size in blocks but the last, whose count the reader
/// bounds rather than knows.
fn pcluster_blocks(img: &[u8], fs: &Filesystem, path: &str) -> Vec<u64> {
    let dev = common::MemDev::new(img.to_vec());
    let inode = fs.lookup_path(path).unwrap();
    let zmap = ZMap::open(&dev as &dyn BlockRead, fs.superblock(), &inode).unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let extent = zmap.pcluster_extent(&dev as &dyn BlockRead, at).unwrap();
        if extent.source_end_byte >= inode.size {
            return out;
        }
        out.push(extent.pcluster_block_count);
        at = extent.source_end_byte;
    }
}

#[test]
fn compressible_files_are_lz4hc_compressed_into_big_pclusters() {
    let data = text(1 << 20, 1);
    let img = mkfs::build_image_with(dir(vec![("init.rc", file(&data))]), 12, options()).unwrap();
    let fs = open_image(img.clone());
    let inode = fs.lookup_path("/init.rc").unwrap();
    assert!(
        matches!(
            inode.format.layout,
            DataLayout::CompressionLegacy | DataLayout::Compression
        ),
        "{:?}",
        inode.format.layout
    );
    assert_eq!(read_back(&fs, "/init.rc"), data);
    assert!(
        (img.len() as u64) < (data.len() as u64) / 2,
        "{} bytes of text built a {}-byte image",
        data.len(),
        img.len()
    );
    let blocks = pcluster_blocks(&img, &fs, "/init.rc");
    assert!(
        blocks.iter().all(|&b| (1..=8).contains(&b)),
        "pclusters of {blocks:?} blocks; -C 32768 allows 1..=8"
    );
    assert!(
        blocks.iter().filter(|&&b| b == 8).count() * 2 > blocks.len(),
        "most pclusters should fill 32 KiB: {blocks:?}"
    );
}

#[test]
fn a_file_nothing_in_which_compresses_is_stored_uncompressed() {
    let data = noise(100_000, 2);
    let img = mkfs::build_image_with(dir(vec![("photo.jpg", file(&data))]), 12, options()).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/photo.jpg").unwrap();
    assert!(
        matches!(
            inode.format.layout,
            DataLayout::FlatPlain | DataLayout::FlatInline
        ),
        "{:?}",
        inode.format.layout
    );
    assert_eq!(read_back(&fs, "/photo.jpg"), data);
}

/// A zip with stored and deflated members: its incompressible stretches
/// are stored a block at a time, the rest compressed, in one file.
#[test]
fn a_file_part_of_which_compresses_keeps_both_kinds_of_pcluster() {
    let mut data = text(200_000, 3);
    data.extend(noise(150_003, 4));
    data.extend(text(90_000, 5));
    let img = mkfs::build_image_with(dir(vec![("app.apk", file(&data))]), 12, options()).unwrap();
    let fs = open_image(img.clone());
    assert_eq!(read_back(&fs, "/app.apk"), data);
    let inode = fs.lookup_path("/app.apk").unwrap();
    assert!(!matches!(
        inode.format.layout,
        DataLayout::FlatPlain | DataLayout::FlatInline
    ));
    let blocks = pcluster_blocks(&img, &fs, "/app.apk");
    assert!(
        blocks.contains(&1),
        "the noise is stored a block at a time: {blocks:?}"
    );
    assert!(
        blocks.contains(&8),
        "the text is compressed into 32 KiB: {blocks:?}"
    );
}

/// Every size the cluster arithmetic treats differently reads back.
#[test]
fn every_size_around_the_block_and_pcluster_boundaries_reads_back() {
    let sizes = [
        0,
        1,
        4095,
        4096,
        4097,
        8191,
        8192,
        32767,
        32768,
        32769,
        98_309,
        1 << 20,
    ];
    let names: Vec<String> = sizes.iter().map(|s| format!("t{s}")).collect();
    let mut entries: Vec<(&str, Node)> = Vec::new();
    for (name, &size) in names.iter().zip(&sizes) {
        entries.push((name.as_str(), file(&text(size, size as u64))));
    }
    let img = mkfs::build_image_with(dir(entries), 12, options()).unwrap();
    let fs = open_image(img);
    for (name, &size) in names.iter().zip(&sizes) {
        assert_eq!(
            read_back(&fs, &format!("/{name}")),
            text(size, size as u64),
            "{name}"
        );
    }
}

#[test]
fn a_compressed_streamed_file_is_read_once() {
    let data = text(300_000, 6);
    let read = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let (r, o, bytes) = (read.clone(), opens.clone(), data.clone());
    struct Counted(Cursor<Vec<u8>>, Arc<AtomicUsize>);
    impl std::io::Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }
    let node = Node::StreamedFile {
        mode: DEFAULT_FILE_MODE,
        content: Content::new(data.len() as u64, move || {
            o.fetch_add(1, Ordering::SeqCst);
            Ok(Counted(Cursor::new(bytes), r))
        }),
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image_with(dir(vec![("f", node)]), 12, options()).unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(read.load(Ordering::SeqCst), data.len());
    assert_eq!(read_back(&open_image(img), "/f"), data);
}

#[test]
fn the_same_tree_compresses_to_the_same_bytes() {
    let tree = || {
        dir(vec![
            ("a", file(&text(100_000, 7))),
            ("b", file(&noise(20_000, 8))),
        ])
    };
    assert_eq!(
        mkfs::build_image_with(tree(), 12, options()).unwrap(),
        mkfs::build_image_with(tree(), 12, options()).unwrap()
    );
}

/// What `fsck.erofs` and the kernel make of it: the image passes a strict
/// check, which decompresses every pcluster, and every file the kernel
/// reads from the mounted image hashes to what went in.
#[test]
fn fsck_accepts_the_compressed_image_and_the_kernel_reads_every_file() {
    use fs_erofs_test_support::{guest_kernel_read, oracle, sha256_hex, ScratchDir};
    let mut mixed = text(200_000, 3);
    mixed.extend(noise(150_003, 4));
    mixed.extend(text(90_000, 5));
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("init.rc", text(1 << 20, 1)),
        ("photo.jpg", noise(100_000, 2)),
        ("app.apk", mixed),
        ("small", text(3000, 9)),
        ("block", text(4096, 10)),
        ("odd", text(98_309, 11)),
    ];
    let tree = dir(files.iter().map(|(n, d)| (*n, file(d))).collect());
    let img = mkfs::build_image_with(tree, 12, options()).unwrap();
    let scratch = ScratchDir::new("lz4hc-writer");
    let path = scratch.join("lz4hc.img");
    std::fs::write(&path, &img).unwrap();
    let image = path.to_str().expect("utf-8 path");

    let fsck = oracle("fsck.erofs").arg(image).output();
    assert!(
        fsck.status.success(),
        "fsck.erofs refused the compressed image:\n{}{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );

    let out = guest_kernel_read(image, r#"cd "$MNT" && sha256sum -- *"#);
    assert!(
        out.status.success(),
        "the kernel could not read the image:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let kernel = String::from_utf8(out.stdout).unwrap();
    for (name, data) in &files {
        let want = format!("{}  {name}", sha256_hex(data));
        assert!(
            kernel.lines().any(|l| l == want),
            "the kernel read {name} as something else:\n{kernel}"
        );
    }
}
