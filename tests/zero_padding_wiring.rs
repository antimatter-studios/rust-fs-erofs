//! The superblock's ZERO_PADDING bit, not a constant, decides whether the
//! decompressor strips a leading pad (#71).
//!
//! The codec tests call `decompress_with_config_and_padding` with a literal
//! flag, so they pin the codec's response to the flag and say nothing about
//! where the flag comes from. Measured on #71: reversing the derivation in
//! `src/fs.rs` or hard-coding it `false` failed the suite loudly, but
//! hard-coding it `true` -- strip unconditionally, the pre-#59 behaviour --
//! left every test green, because every compressed image anyone writes sets
//! the bit.
//!
//! So this builds the one case that direction needs: an image with the bit
//! CLEAR whose compressed frame legitimately BEGINS with a zero byte. A raw
//! DEFLATE stream opening with a non-final stored block does exactly that
//! (first byte `0x00`: BFINAL=0, BTYPE=00). Read through `Filesystem`, the
//! leading zero must survive as part of the frame. The superblock checksum
//! is recomputed after the bit is cleared, so the case stays valid once the
//! checksum is verified (#52).

mod common;
use common::{dir, MemDev};

use fs_core::BlockRead;
use fs_erofs::superblock::EROFS_FEATURE_INCOMPAT_ZERO_PADDING;
use fs_erofs::zmap::{ZMap, Z_EROFS_LCLUSTER_TYPE_PLAIN};
use fs_erofs::{mkfs, Filesystem};
use std::sync::Arc;

const SB: usize = 1024;
const CHECKSUM_AT: usize = SB + 0x04;
const FEATURE_COMPAT_AT: usize = SB + 0x08;
const BLKSZBITS_AT: usize = SB + 0x0C;
const FEATURE_INCOMPAT_AT: usize = SB + 0x50;
const BS: usize = 4096;

fn data() -> Vec<u8> {
    // Compressible, so the writer lays it out as a compressed pcluster
    // rather than PLAIN.
    b"zero padding is decided by the superblock\n"
        .iter()
        .copied()
        .cycle()
        .take(3000)
        .collect()
}

/// Raw DEFLATE: one non-final stored block holding `payload`, then an empty
/// final stored block. Starts with `0x00`.
fn stored_deflate(payload: &[u8]) -> Vec<u8> {
    let len = u16::try_from(payload.len()).expect("fits one stored block");
    let mut out = vec![0x00];
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    out
}

fn u32_at(img: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(img[at..at + 4].try_into().unwrap())
}

/// An image whose one compressed file's pcluster holds `stored_deflate`
/// LEFT-aligned (no pad), with ZERO_PADDING cleared.
fn unpadded_image() -> Vec<u8> {
    let tree = dir(vec![(
        "f.txt",
        mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
            mode: mkfs::DEFAULT_FILE_MODE,
            data: data(),
            algo: mkfs::CompressedAlgo::Deflate,
            lclusterbits: 0,
            meta: mkfs::NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: mkfs::CompressedFileSpec::default_index_format(),
            ztailpacking: false,
            target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
        }),
    )]);
    let mut img = mkfs::build_image(tree, 12).expect("build image");
    assert_ne!(
        u32_at(&img, FEATURE_INCOMPAT_AT) & EROFS_FEATURE_INCOMPAT_ZERO_PADDING,
        0,
        "the writer is expected to set ZERO_PADDING on a compressed image"
    );

    // Where the file's one pcluster lives, asked of the reader's own map.
    let blkaddr = {
        let fs = Filesystem::open(Arc::new(MemDev::new(img.clone())) as Arc<dyn BlockRead>)
            .expect("open the unmodified image");
        let inode = fs.lookup_path("/f.txt").expect("lookup");
        let dev = MemDev::new(img.clone());
        let zmap = ZMap::open(&dev, fs.superblock(), &inode).expect("zmap");
        let extent = zmap.pcluster_extent(&dev, 0).expect("extent");
        assert_ne!(
            extent.cluster_type, Z_EROFS_LCLUSTER_TYPE_PLAIN,
            "not compressed"
        );
        assert_eq!(
            (extent.source_start_byte, extent.source_end_byte),
            (0, data().len() as u64),
            "expected one pcluster covering the whole file"
        );
        extent.pcluster_blkaddr as usize
    };

    let frame = stored_deflate(&data());
    assert!(frame.len() <= BS);
    let block = &mut img[blkaddr * BS..(blkaddr + 1) * BS];
    block.fill(0);
    block[..frame.len()].copy_from_slice(&frame);

    let incompat = u32_at(&img, FEATURE_INCOMPAT_AT) & !EROFS_FEATURE_INCOMPAT_ZERO_PADDING;
    img[FEATURE_INCOMPAT_AT..FEATURE_INCOMPAT_AT + 4].copy_from_slice(&incompat.to_le_bytes());
    if u32_at(&img, FEATURE_COMPAT_AT) & 1 != 0 {
        // CRC32C over the superblock to the end of its block, checksum
        // field zeroed, no final XOR -- `Superblock::verify_checksum`.
        let block_size = 1usize << img[BLKSZBITS_AT];
        let want_len = block_size - SB % block_size;
        let mut span = img[SB..SB + want_len].to_vec();
        span[4..8].fill(0);
        let crc = crc32c::crc32c(&span) ^ 0xFFFF_FFFF;
        img[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&crc.to_le_bytes());
    }
    img
}

#[test]
fn with_zero_padding_clear_a_leading_zero_is_part_of_the_frame() {
    let img = unpadded_image();
    let fs = Filesystem::open(Arc::new(MemDev::new(img)) as Arc<dyn BlockRead>)
        .expect("open the unpadded image");
    assert_eq!(
        fs.superblock().feature_incompat & EROFS_FEATURE_INCOMPAT_ZERO_PADDING,
        0,
        "control: the image under test really has the bit clear"
    );
    let inode = fs.lookup_path("/f.txt").expect("lookup");
    let mut buf = vec![0u8; inode.size as usize];
    let read = fs.read_file(&inode, 0, &mut buf);
    assert!(
        read.is_ok() && buf == data(),
        "an image without ZERO_PADDING had its frame's leading 0x00 stripped as pad: \
         read_file -> {read:?}, contents match = {}",
        buf == data()
    );
}

/// Control: the frame itself is valid raw DEFLATE for the file's bytes, so
/// a failure above is about the padding decision and not about the frame.
#[test]
fn control_the_stored_block_frame_is_valid_deflate() {
    let mut out = vec![0u8; data().len()];
    fs_erofs::decompress::decompress_with_config_and_padding(
        fs_erofs::Algorithm::Deflate,
        None,
        false,
        &stored_deflate(&data()),
        &mut out,
    )
    .expect("the frame decodes with padding off");
    assert_eq!(out, data());
}
