//! EROFS chunk-based data layout (`DataLayout::ChunkBased`, layout id 4).
//!
//! Chunked files are split into fixed-size chunks of `block_size <<
//! chunk_bits` bytes. Each chunk is independently placed on disk; the
//! per-inode "chunk map" (immediately following the inode body + inline
//! xattrs) lists where every chunk lives. Holes are represented by the
//! sentinel block address `EROFS_NULL_ADDR`.
//!
//! Two chunk-map shapes exist, picked by the `EROFS_CHUNK_FORMAT_INDEXES`
//! bit in the per-layout flags:
//!
//! - **compact** (flag clear): packed `__le32` block addresses, 4 bytes
//!   per entry.
//! - **indexed** (flag set): packed `struct erofs_inode_chunk_index` (8
//!   bytes: `advise:u16`, `device_id:u16`, `blkaddr:u32`). `advise`
//!   stays reserved/ignored; `device_id` is surfaced to the caller so
//!   multi-device images can route reads through the correct backing
//!   device (`device_id == 0` -> primary, `>= 1` -> the matching slot
//!   in the SB device table).
//!
//! Sources: EROFS on-disk format documentation
//! (<https://erofs.docs.kernel.org/en/latest/design.html>) and the
//! `EROFS_CHUNK_FORMAT_*` bit definitions in the public format header
//! `erofs_fs.h`.

use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::superblock::Superblock;
use fs_core::BlockRead;

/// Hole sentinel: a chunk whose recorded block address equals this is
/// not present on disk; reads return zeros.
pub const EROFS_NULL_ADDR: u32 = 0xFFFF_FFFF;

/// Low 5 bits of `format.flags` carry log2(chunk_size / block_size).
pub const EROFS_CHUNK_FORMAT_BLKBITS_MASK: u16 = 0x1F;

/// Bit 5 of `format.flags`: chunk-map entries are 8-byte
/// `erofs_inode_chunk_index` instead of bare `__le32` block addresses.
pub const EROFS_CHUNK_FORMAT_INDEXES: u16 = 0x20;

/// Decoded chunk geometry for a chunked inode.
#[derive(Debug, Clone, Copy)]
pub struct ChunkInfo {
    /// log2(chunk_size_in_blocks). chunk_size = block_size << chunk_bits.
    pub chunk_bits: u8,
    /// `true` => 8-byte indexed entries, `false` => 4-byte compact entries.
    pub uses_indexes: bool,
    /// Chunk size in bytes.
    pub chunk_size: u64,
    /// Number of chunks (ceil(file_size / chunk_size)).
    pub n_chunks: u64,
}

/// Derive chunk geometry from the inode's `i_u` chunk-format word + size.
///
/// Spec: `linux/fs/erofs/erofs_fs.h::erofs_inode_chunk_info`. The
/// chunk-format word is the low 16 bits of `i_u`, at offset 0x10, and
/// that is the only place it is read from.
///
/// It used to fall back to `i_format`'s per-layout flags when `i_u`'s
/// low word was zero — a shim for fixtures an older iteration of this
/// crate wrote, which had put the same bits there by accident. The
/// discriminator does not work, because **zero is a legal value of the
/// chunk-format word**: `chunk_bits == 0` means one chunk is one block
/// and a clear `EROFS_CHUNK_FORMAT_INDEXES` means four-byte compact
/// entries, which is an ordinary combination — the crate's own
/// `chunk_info_compact_form` test constructs it. For such an inode the
/// geometry came from a different field entirely.
///
/// The two are not interchangeable. `EROFS_CHUNK_FORMAT_INDEXES` is
/// 0x20, which as an `i_format` flag is bit 9 of the inode's format
/// word, and those bits are per-layout and undefined for a chunked
/// inode. A producer setting anything up there made this reader
/// conclude the chunkmap holds 8-byte indexed entries where it holds
/// 4-byte compact ones: every chunk address is then read from the wrong
/// offset and `lookup_chunk_blkaddr` returns a plausible `u32` from the
/// middle of the neighbouring entry. There is no checksum on a chunkmap
/// and no sentinel, so the read succeeds and returns the wrong blocks.
/// The `chunk_bits` half is quieter and no better: a wrong shift moves
/// `n_chunks` and `chunk_idx` together, so offsets map to entirely
/// different chunks.
///
/// Measured against erofs-utils 1.9.4, which never produces the
/// ambiguous case because it clamps `--chunksize` up to four blocks:
/// `i_format.flags` is zero on every image it writes and `i_u` never
/// is. So the fallback was unreachable through the reference tool —
/// a branch that is wrong and unreachable, which reads as verified.
pub fn chunk_info(sb: &Superblock, inode: &Inode) -> Result<ChunkInfo> {
    let flags = (inode.raw_u & 0xFFFF) as u16;
    let chunk_bits = (flags & EROFS_CHUNK_FORMAT_BLKBITS_MASK) as u8;
    let uses_indexes = (flags & EROFS_CHUNK_FORMAT_INDEXES) != 0;
    // chunk_size = block_size << chunk_bits. Guard against absurd shifts
    // that would overflow u64 -- block_size is at most 1<<16, so
    // chunk_bits up to 47 fits. The 5-bit mask caps chunk_bits at 31.
    let block_size = sb.block_size();
    let chunk_size = block_size
        .checked_shl(chunk_bits as u32)
        .ok_or(Error::BadInode("chunk_bits shift overflow"))?;
    if chunk_size == 0 {
        return Err(Error::BadInode("chunk_size is zero"));
    }
    let n_chunks = inode.size.div_ceil(chunk_size);
    Ok(ChunkInfo {
        chunk_bits,
        uses_indexes,
        chunk_size,
        n_chunks,
    })
}

/// Read the chunk-map entry for `chunk_idx` and return its raw block
/// address. Caller compares against `EROFS_NULL_ADDR` to detect holes.
///
/// For the indexed (8-byte) form the `device_id` field is also surfaced
/// so multi-device callers can route reads to the right backing
/// device. The compact (4-byte) form has no `device_id` slot; it's
/// always reported as 0 (primary device).
///
/// Spec: `struct erofs_inode_chunk_index { __le16 advise; __le16
/// device_id; __le32 blkaddr; }` per the public EROFS on-disk-format
/// documentation.
pub fn lookup_chunk_blkaddr<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    inode: &Inode,
    chunk_idx: u64,
) -> Result<(u32, u16)> {
    let info = chunk_info(sb, inode)?;
    if chunk_idx >= info.n_chunks {
        return Err(Error::OutOfRange);
    }
    let entry_size: u64 = if info.uses_indexes { 8 } else { 4 };
    let map_start = inode.body_end(sb);
    let entry_off = map_start + chunk_idx * entry_size;

    if info.uses_indexes {
        let mut buf = [0u8; 8];
        dev.read_at(entry_off, &mut buf)?;
        // struct erofs_inode_chunk_index { __le16 advise; __le16 device_id; __le32 blkaddr; }
        // We surface `device_id` for multi-device routing; `advise` is
        // currently reserved (unused by the kernel reader) so we drop it.
        let device_id = u16::from_le_bytes(buf[2..4].try_into().unwrap());
        let blkaddr = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        Ok((blkaddr, device_id))
    } else {
        let mut buf = [0u8; 4];
        dev.read_at(entry_off, &mut buf)?;
        Ok((u32::from_le_bytes(buf), 0))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::inode::tests::synth_compact;
    use crate::layout::DataLayout;
    use crate::superblock::tests::synth_sb;
    use crate::test_device::MemDev;

    /// Build a synthetic compact ChunkBased inode carrying `chunk_format`
    /// where the spec puts it: the low 16 bits of `i_u`, at 0x10.
    ///
    /// These fixtures used to write it into `i_format`'s per-layout
    /// flags instead, which is where an older iteration of this crate
    /// read it from — so the tests agreed with the reader and both
    /// disagreed with the format. `format_flags` exists so a test can
    /// put something in that field and assert it is ignored.
    fn synth_chunked_compact(mode: u16, size: u32, chunk_format: u16) -> [u8; 32] {
        synth_chunked_compact_with_format_flags(mode, size, chunk_format, 0)
    }

    fn synth_chunked_compact_with_format_flags(
        mode: u16,
        size: u32,
        chunk_format: u16,
        format_flags: u16,
    ) -> [u8; 32] {
        let mut b = synth_compact(DataLayout::ChunkBased, mode, size, 0);
        let raw_format: u16 = ((DataLayout::ChunkBased as u16) << 1) | (format_flags << 4);
        b[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        b[0x10..0x14].copy_from_slice(&u32::from(chunk_format).to_le_bytes());
        b
    }

    #[test]
    fn chunk_info_compact_form() {
        // chunk_bits = 1 -> chunk_size = block_size * 2 = 8 KiB.
        // INDEXES bit clear.
        let inode_buf = synth_chunked_compact(0x81A4, 16384, 1);
        let inode = Inode::parse(0, &inode_buf).unwrap();
        let sb_buf = synth_sb(12, 0, 1, 16);
        let sb = Superblock::parse(&sb_buf).unwrap();
        let info = chunk_info(&sb, &inode).unwrap();
        assert_eq!(info.chunk_bits, 1);
        assert!(!info.uses_indexes);
        assert_eq!(info.chunk_size, 8192);
        assert_eq!(info.n_chunks, 2); // 16384 / 8192
    }

    /// The geometry comes from `i_u` even when `i_format`'s per-layout
    /// flags say something else.
    ///
    /// This is the case the old fallback got wrong: a chunk-format word
    /// of zero is legal — one block per chunk, compact entries — and it
    /// made the reader take the geometry from a field that means
    /// nothing here. The `i_format` flags below spell `INDEXES` set and
    /// `chunk_bits = 3`; both must be ignored.
    #[test]
    fn chunk_geometry_ignores_the_per_layout_format_flags() {
        let misleading = EROFS_CHUNK_FORMAT_INDEXES | 3;
        let inode_buf = synth_chunked_compact_with_format_flags(0x81A4, 8192, 0, misleading);
        let inode = Inode::parse(0, &inode_buf).unwrap();
        assert_eq!(
            inode.format.flags, misleading,
            "the fixture did not put the misleading value where it meant to"
        );
        let sb_buf = synth_sb(12, 0, 1, 16);
        let sb = Superblock::parse(&sb_buf).unwrap();

        let info = chunk_info(&sb, &inode).unwrap();
        assert_eq!(info.chunk_bits, 0, "chunk_bits came from i_format");
        assert!(!info.uses_indexes, "the INDEXES bit came from i_format");
        // One block per chunk, and the harness's block size is 4 KiB.
        assert_eq!(info.chunk_size, 4096);
        assert_eq!(info.n_chunks, 2);
    }

    /// A chunk-format word of zero is a geometry, not an absence.
    ///
    /// One block per chunk with compact entries — what
    /// `mkfs.erofs --chunksize=<one block>` would write, and what the
    /// old discriminator read as "nothing here, look elsewhere".
    #[test]
    fn a_chunk_format_word_of_zero_is_one_block_per_chunk() {
        let inode_buf = synth_chunked_compact(0x81A4, 3 * 4096, 0);
        let inode = Inode::parse(0, &inode_buf).unwrap();
        let sb_buf = synth_sb(12, 0, 1, 16);
        let sb = Superblock::parse(&sb_buf).unwrap();
        let info = chunk_info(&sb, &inode).unwrap();
        assert_eq!(info.chunk_bits, 0);
        assert!(!info.uses_indexes);
        assert_eq!(info.chunk_size, 4096);
        assert_eq!(info.n_chunks, 3);
    }

    #[test]
    fn chunk_info_indexed_form() {
        // chunk_bits = 0 (chunk == 1 block). INDEXES bit set (0x20 in flags).
        let flags = EROFS_CHUNK_FORMAT_INDEXES; // 0x20
        let inode_buf = synth_chunked_compact(0x81A4, 4097, flags);
        let inode = Inode::parse(0, &inode_buf).unwrap();
        let sb_buf = synth_sb(12, 0, 1, 16);
        let sb = Superblock::parse(&sb_buf).unwrap();
        let info = chunk_info(&sb, &inode).unwrap();
        assert_eq!(info.chunk_bits, 0);
        assert!(info.uses_indexes);
        assert_eq!(info.chunk_size, 4096);
        assert_eq!(info.n_chunks, 2); // ceil(4097/4096)
    }

    /// Build an image whose meta area at block 1 contains:
    ///   NID 0: chunked compact inode with two-entry compact chunkmap
    ///          inline immediately after.
    /// Returns (image bytes, sb, inode-nid).
    fn build_compact_chunkmap_image(blkaddrs: [u32; 2]) -> Vec<u8> {
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 4];
        let sb = synth_sb(12, 0, 1, 4);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);
        // chunk_bits=0 (chunk == 1 block), INDEXES clear, two chunks needed
        // for a file of size 4097..=8192.
        let inode_buf = synth_chunked_compact(0x81A4, 8192, 0);
        // Inode at NID 0 -> byte 4096 (meta_blkaddr=1).
        img[BS..BS + 32].copy_from_slice(&inode_buf);
        // Compact chunkmap entries directly after inode (no xattrs):
        let map_off = BS + 32;
        img[map_off..map_off + 4].copy_from_slice(&blkaddrs[0].to_le_bytes());
        img[map_off + 4..map_off + 8].copy_from_slice(&blkaddrs[1].to_le_bytes());
        img
    }

    #[test]
    fn lookup_compact_chunkmap_present_and_hole() {
        let img = build_compact_chunkmap_image([0x1234_5678, EROFS_NULL_ADDR]);
        let dev = MemDev::new(img);
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let (a0, d0) = lookup_chunk_blkaddr(&dev, &sb, &inode, 0).unwrap();
        let (a1, d1) = lookup_chunk_blkaddr(&dev, &sb, &inode, 1).unwrap();
        assert_eq!(a0, 0x1234_5678);
        assert_eq!(a1, EROFS_NULL_ADDR);
        // Compact (4-byte) form has no device_id slot; always 0.
        assert_eq!(d0, 0);
        assert_eq!(d1, 0);
    }

    #[test]
    fn lookup_indexed_chunkmap() {
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 4];
        let sb = synth_sb(12, 0, 1, 4);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);
        // INDEXES set, chunk_bits=0; two chunks for size 8192.
        let inode_buf = synth_chunked_compact(0x81A4, 8192, EROFS_CHUNK_FORMAT_INDEXES);
        img[BS..BS + 32].copy_from_slice(&inode_buf);
        // 8-byte entries: advise=0, device_id=0, blkaddr=...
        let map_off = BS + 32;
        // entry 0: blkaddr = 7
        img[map_off..map_off + 2].copy_from_slice(&0u16.to_le_bytes()); // advise
        img[map_off + 2..map_off + 4].copy_from_slice(&0u16.to_le_bytes()); // device_id
        img[map_off + 4..map_off + 8].copy_from_slice(&7u32.to_le_bytes()); // blkaddr
                                                                            // entry 1: hole
        img[map_off + 8..map_off + 10].copy_from_slice(&0u16.to_le_bytes());
        img[map_off + 10..map_off + 12].copy_from_slice(&0u16.to_le_bytes());
        img[map_off + 12..map_off + 16].copy_from_slice(&EROFS_NULL_ADDR.to_le_bytes());

        let dev = MemDev::new(img);
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        assert_eq!(lookup_chunk_blkaddr(&dev, &sb, &inode, 0).unwrap(), (7, 0));
        assert_eq!(
            lookup_chunk_blkaddr(&dev, &sb, &inode, 1).unwrap(),
            (EROFS_NULL_ADDR, 0)
        );
    }

    #[test]
    fn indexed_chunkmap_with_extra_device() {
        // Indexed-form chunkmap with non-zero device_id on each chunk.
        // chunk 0 -> device 1 / blkaddr 9, chunk 1 -> device 2 / blkaddr 17.
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 4];
        let sb = synth_sb(12, 0, 1, 4);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);
        let inode_buf = synth_chunked_compact(0x81A4, 8192, EROFS_CHUNK_FORMAT_INDEXES);
        img[BS..BS + 32].copy_from_slice(&inode_buf);
        let map_off = BS + 32;
        // entry 0: device_id=1, blkaddr=9
        img[map_off..map_off + 2].copy_from_slice(&0u16.to_le_bytes()); // advise
        img[map_off + 2..map_off + 4].copy_from_slice(&1u16.to_le_bytes()); // device_id
        img[map_off + 4..map_off + 8].copy_from_slice(&9u32.to_le_bytes()); // blkaddr
                                                                            // entry 1: device_id=2, blkaddr=17
        img[map_off + 8..map_off + 10].copy_from_slice(&0u16.to_le_bytes());
        img[map_off + 10..map_off + 12].copy_from_slice(&2u16.to_le_bytes());
        img[map_off + 12..map_off + 16].copy_from_slice(&17u32.to_le_bytes());

        let dev = MemDev::new(img);
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let (a0, d0) = lookup_chunk_blkaddr(&dev, &sb, &inode, 0).unwrap();
        let (a1, d1) = lookup_chunk_blkaddr(&dev, &sb, &inode, 1).unwrap();
        assert_eq!((a0, d0), (9, 1));
        assert_eq!((a1, d1), (17, 2));
    }

    #[test]
    fn lookup_out_of_range_chunk() {
        let img = build_compact_chunkmap_image([5, 6]);
        let dev = MemDev::new(img);
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        assert!(matches!(
            lookup_chunk_blkaddr(&dev, &sb, &inode, 2),
            Err(Error::OutOfRange)
        ));
    }
}
