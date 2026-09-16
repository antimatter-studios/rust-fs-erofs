//! A `feature_incompat` bit this reader does not implement refuses the
//! image, by name, at open.
//!
//! The point of an incompatible-feature bit is that a reader which does
//! not understand it must not guess. Before #51 nothing looked at the
//! word: `mkfs.erofs -m65536` (metabox, `0x100`, with 48-bit `0x80`)
//! opened as a healthy volume, resolved the root to NID 0 -- the boot
//! area -- and failed several layers later with `NotADirectory`, and an
//! image whose boot-area bytes happened to parse as a directory would
//! have been walked into unrelated data. `-E48bit` (#45) opened and read
//! at the sizes that could be built, by the coincidence that the high
//! half of every block address was zero.
//!
//! Two halves. The first builds an image with this crate's writer and
//! sets one bit in its superblock, recomputing the superblock checksum so
//! the case stays valid once the checksum is verified (#52). The second,
//! `#[ignore]`-gated with the other oracles, asks the real `mkfs.erofs`.

mod common;
use common::{build_with_mkfs_erofs, dir, file, mkfs_erofs_available, open_image, MemDev};

use fs_core::BlockRead;
use fs_erofs::{mkfs, Error, Filesystem};
use std::sync::Arc;

const SB: usize = 1024;
const FEATURE_COMPAT_AT: usize = SB + 0x08;
const CHECKSUM_AT: usize = SB + 0x04;
const BLKSZBITS_AT: usize = SB + 0x0C;
const FEATURE_INCOMPAT_AT: usize = SB + 0x50;

fn tree() -> mkfs::Node {
    dir(vec![
        ("hello.txt", file(b"hello, world\n")),
        ("sub", dir(vec![("inner.txt", file(b"inner\n"))])),
    ])
}

/// This crate's own image with `bits` ORed into `feature_incompat` and
/// the superblock checksum recomputed if the image carries one.
fn image_with_incompat(bits: u32) -> Vec<u8> {
    let mut img = mkfs::build_image(tree(), 12).expect("build image");
    let at = FEATURE_INCOMPAT_AT..FEATURE_INCOMPAT_AT + 4;
    let was = u32::from_le_bytes(img[at.clone()].try_into().unwrap());
    img[at].copy_from_slice(&(was | bits).to_le_bytes());

    let compat = u32::from_le_bytes(
        img[FEATURE_COMPAT_AT..FEATURE_COMPAT_AT + 4]
            .try_into()
            .unwrap(),
    );
    if compat & 1 != 0 {
        let block_size = 1usize << img[BLKSZBITS_AT];
        // CRC32C over the superblock to the end of its block, checksum
        // field zeroed, no final XOR -- `Superblock::verify_checksum`.
        let want_len = block_size - SB % block_size;
        let mut span = img[SB..SB + want_len].to_vec();
        span[4..8].fill(0);
        let crc = crc32c::crc32c(&span) ^ 0xFFFF_FFFF;
        img[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&crc.to_le_bytes());
    }
    img
}

fn open_bytes(img: Vec<u8>) -> Result<Filesystem, Error> {
    Filesystem::open(Arc::new(MemDev::new(img)) as Arc<dyn BlockRead>)
}

/// `open` must refuse, with a superblock error whose reason names `name`.
fn refused_naming(bits: u32, name: &str) {
    match open_bytes(image_with_incompat(bits)) {
        Err(Error::BadSuperblock(why)) => assert!(
            why.contains(name),
            "feature_incompat {bits:#x} was refused, but the reason {why:?} does not name {name:?}"
        ),
        Err(other) => panic!("feature_incompat {bits:#x} refused for the wrong reason: {other:?}"),
        Ok(fs) => panic!(
            "feature_incompat {bits:#x} ({name}) is not implemented by this reader, and the image \
             opened anyway (root nid {})",
            fs.superblock().root_nid
        ),
    }
}

#[test]
fn metabox_is_refused_by_name() {
    refused_naming(0x0000_0100, "metabox");
}

#[test]
fn forty_eight_bit_addressing_is_refused_by_name() {
    refused_naming(0x0000_0080, "48bit");
}

#[test]
fn a_bit_no_reader_knows_is_refused() {
    refused_naming(0x8000_0000, "unknown");
}

/// The negative control: the bits this reader does implement are claimed
/// deliberately, so the gate cannot over-correct into refusing them.
/// COMPR_CFGS (0x02) and DEVICE_TABLE (0x08) are claimed too, but setting
/// them on an image with no blob or table describes a different, corrupt
/// image, so they are covered by the oracle half instead.
#[test]
fn implemented_bits_still_open_and_read() {
    for bits in [0x01, 0x04, 0x10, 0x20, 0x40] {
        let fs = open_bytes(image_with_incompat(bits))
            .unwrap_or_else(|e| panic!("feature_incompat {bits:#x} is implemented, yet: {e}"));
        let inode = fs.lookup_path("/sub/inner.txt").expect("lookup");
        let mut buf = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf).expect("read");
        assert_eq!(buf, b"inner\n", "feature_incompat {bits:#x}");
    }
    // And the untouched image, so a failure above is about the bit.
    let fs = open_image(image_with_incompat(0));
    assert!(fs.lookup_path("/hello.txt").is_ok());
}

/// What the real tool writes: chunked files open and read; 48-bit
/// addressing is refused by name.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_chunked_opens_and_48bit_is_refused() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let chunked = build_with_mkfs_erofs(&["--chunksize=65536"], &tree());
    let fs = open_bytes(chunked.bytes).expect("a --chunksize image opens");
    let inode = fs.lookup_path("/hello.txt").expect("lookup");
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).expect("read");
    assert_eq!(buf, b"hello, world\n");

    let wide = build_with_mkfs_erofs(&["-E48bit"], &tree());
    match open_bytes(wide.bytes) {
        Err(Error::BadSuperblock(why)) => assert!(why.contains("48bit"), "{why}"),
        Err(other) => panic!("-E48bit refused for the wrong reason: {other:?}"),
        Ok(_) => panic!("-E48bit opened; this reader does not implement 48-bit addressing"),
    }
}
