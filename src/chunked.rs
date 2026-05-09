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
/// chunk-format word is the low 16 bits of i_u (offset 0x10). Older
/// reader iterations of this crate read the same bits from `i_format`'s
/// per-layout flags by accident -- they happened to match for
/// chunk_bits=0 + INDEXES-clear images. This implementation reads from
/// the spec-correct i_u location and tolerates either as a fallback so
/// older fixtures still parse.
pub fn chunk_info(sb: &Superblock, inode: &Inode) -> Result<ChunkInfo> {
    let cf = (inode.raw_u & 0xFFFF) as u16;
    let from_iu = cf != 0;
    let flags = if from_iu { cf } else { inode.format.flags };
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
    use fs_core::{BlockRead, Result as BlockResult};
    use std::sync::Mutex;

    /// In-memory device for tests.
    pub(crate) struct MemDev(pub Mutex<Vec<u8>>);
    impl BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> BlockResult<()> {
            let v = self.0.lock().unwrap();
            let start = offset as usize;
            let end = start + buf.len();
            if end > v.len() {
                return Err(fs_core::Error::ShortRead {
                    offset,
                    want: buf.len(),
                    got: v.len().saturating_sub(start),
                });
            }
            buf.copy_from_slice(&v[start..end]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
    }

    /// Build a synthetic compact ChunkBased inode buffer with a given
    /// per-layout `flags` value. Layout occupies bits 1..=3, flags occupy
    /// bits 4..=15 of `i_format`.
    fn synth_chunked_compact(mode: u16, size: u32, flags: u16) -> [u8; 32] {
        let mut b = synth_compact(DataLayout::ChunkBased, mode, size, 0);
        // Re-pack i_format with flags. synth_compact wrote
        // raw_format = (DataLayout::ChunkBased as u16) << 1; we OR in flags << 4.
        let raw_format: u16 = ((DataLayout::ChunkBased as u16) << 1) | (flags << 4);
        b[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
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
        let dev = MemDev(Mutex::new(img));
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

        let dev = MemDev(Mutex::new(img));
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

        let dev = MemDev(Mutex::new(img));
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
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        assert!(matches!(
            lookup_chunk_blkaddr(&dev, &sb, &inode, 2),
            Err(Error::OutOfRange)
        ));
    }
}
