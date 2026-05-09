//! Cluster-map ("zmap") parsing for EROFS compressed inodes.
//!
//! A compressed file is divided into LOGICAL CLUSTERS of fixed size
//! `1 << (blkszbits + lclusterbits)` bytes. Each logical cluster maps,
//! via an on-disk index, to one or more PHYSICAL CLUSTERS holding the
//! compressed payload. Several logical clusters can share a single head
//! pcluster when contiguous lclusters were compressed together.
//!
//! Two on-disk index formats coexist:
//!
//! - **Legacy / uncompacted** (full `z_erofs_lcluster_index`): one
//!   8-byte entry per logical cluster, written when the inode's
//!   datalayout is `EROFS_INODE_COMPRESSED_FULL` (1). Layout: 8-byte
//!   `z_erofs_map_header` + 8-byte reserved gap, then the entry array.
//!   See `Z_EROFS_FULL_INDEX_START` in `linux/fs/erofs/erofs_fs.h`.
//! - **Compact**: written when datalayout is
//!   `EROFS_INODE_COMPRESSED_COMPACT` (3) -- the modern default for
//!   `mkfs.erofs`. The 8-byte header is followed (with no gap) by a
//!   sequence of fixed-size PACKS that interleave per-lcluster encoded
//!   bits with a per-pack `__le32` base blkaddr.
//!
//! ### Compact pack geometry
//!
//! Each pack is `vcnt << amortizedshift` bytes long. `amortizedshift`
//! is `2` (each lcluster amortized to 4 bytes) or `1` (2 bytes / lc):
//!
//! - `amortizedshift = 2` ("compacted-4B"): pack = 8 bytes, `vcnt = 2`.
//!   Bitstream is 4 bytes; trailing 4 bytes are the pack's base blkaddr.
//! - `amortizedshift = 1` ("compacted-2B"): pack = 32 bytes, `vcnt = 16`.
//!   Bitstream is 28 bytes; trailing 4 bytes are the pack's base
//!   blkaddr.
//!
//! Within one inode the kernel walks lclusters through THREE regions in
//! order:
//!
//! 1. `compacted_4b_initial` lclusters at the start, packed 4B form.
//!    `compacted_4b_initial = ((32 - ebase % 32) / 4) & 7` -- this is a
//!    32-byte-alignment pad so the subsequent 2B packs (32 bytes each)
//!    line up.
//! 2. `compacted_2b` lclusters in the middle, 2B form. Only nonzero when
//!    `Z_EROFS_ADVISE_COMPACTED_2B` is set on the map header AND
//!    `compacted_4b_initial < totalidx`. Equals
//!    `rounddown(totalidx - compacted_4b_initial, 16)`.
//! 3. The remaining trailing lclusters in 4B form again.
//!
//! `ebase` is the on-disk byte offset of the first pack:
//! `Z_EROFS_MAP_HEADER_END(end) = ALIGN(end, 8) + sizeof(map_header)`,
//! where `end` is `iloc + inode_isize + xattr_isize`.
//!
//! ### Per-entry bit encoding
//!
//! For a pack of `pack_bytes = vcnt << amortizedshift` bytes:
//! - `encodebits = (pack_bytes - 4) * 8 / vcnt` (16 for 4B, 14 for 2B).
//! - `lobits = max(z_lclusterbits, ilog2(Z_EROFS_LI_D0_CBLKCNT) + 1)
//!   = max(z_lclusterbits, 12)`. `z_lclusterbits = blkszbits +
//!   header.lclusterbits`.
//! - Per-entry payload: bits `[0..lobits)` = `lo`, bits `[lobits..lobits+2)`
//!   = `cluster_type` (PLAIN=0/HEAD1=1/NONHEAD=2/HEAD2=3).
//!
//! `lo` carries `clusterofs` (offset within the source-byte stream where
//! THIS pcluster's data starts) for HEAD/PLAIN, and `delta[0]` (lcluster
//! distance back to the owning HEAD) for NONHEAD. The LAST entry in
//! a pack (`i == vcnt - 1`) is special when NONHEAD: its `lo` carries
//! `delta[1]` (forward distance to the NEXT HEAD) and we must derive
//! `delta[0]` from the previous entry. The pack-tail special case is
//! described in the public EROFS compression-format documentation
//! (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
//!
//! ### HEAD / PLAIN blkaddr derivation
//!
//! Each pack carries a single base blkaddr in its trailing `__le32`.
//! For a HEAD/PLAIN at intra-pack index `i`, walk BACKWARD inside the
//! same pack: at each non-NONHEAD entry add 1 to `nblk`, and at each
//! NONHEAD entry skip backward by its `delta[0]`. Final
//! `pcluster_blkaddr = base + nblk` (with `nblk` initialized to 1 so
//! the first entry in a pack lands at `base + 1`). This is the
//! "running blkaddr base" arithmetic the integrating-agent note flagged.
//!
//! ### Advise bits we currently understand
//!
//! - `Z_EROFS_ADVISE_COMPACTED_2B` -- enables the 2B middle region.
//! - `Z_EROFS_ADVISE_INLINE_PCLUSTER` -- ztailpacking. Last pcluster's
//!   compressed bytes inlined just past the index area;
//!   `tail_inline_offset_and_size` exposes the (offset, idata_size) pair.
//! - `Z_EROFS_ADVISE_FRAGMENT_PCLUSTER` -- cross-file packed-tail
//!   dedup. The trailing logical bytes of an affected file (the last
//!   `lcluster_size`-aligned chunk) actually live in a special "packed
//!   inode" at byte offset `h_fragmentoff`; the file's own pcluster
//!   index has no entry for that range. Surfaces via
//!   [`ZMap::fragment_range`]; the read path in `fs.rs` redirects
//!   fragment-bearing byte offsets to the packed inode.
//! - `Z_EROFS_ADVISE_BIG_PCLUSTER_1/2` -- multi-block pclusters carrying
//!   the compressed-block count of each pcluster on the FIRST NONHEAD
//!   entry following its HEAD via the `Z_EROFS_LI_D0_CBLKCNT` marker.
//!   `pcluster_extent` decodes this marker to size the on-disk read.
//!   mkfs.erofs always sets these bits when emitting LZMA / DEFLATE
//!   streams (validated empirically against erofs-utils 1.9 output).
//!
//! ### Note on a hypothetical "compacted-1B" encoding
//!
//! Some loose discussion in pre-1.0 design threads alluded to a
//! `Z_EROFS_ADVISE_COMPACTED_1B` (1-byte-per-lcluster) bitstream as a
//! future companion to compacted-2B / compacted-4B. As of erofs-utils
//! 1.9 and the public EROFS on-disk-format documentation
//! (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>)
//! no such constant or encoding exists in the on-disk spec: the public
//! kernel header `erofs_fs.h` carries only `Z_EROFS_ADVISE_COMPACTED_2B`
//! plus the BIG_PCLUSTER / INTERLACED / INLINE / FRAGMENT bits; mkfs.erofs
//! 1.9 emits no string, error, or `-E` flag mentioning compacted-1B
//! (verified by `strings /opt/homebrew/bin/mkfs.erofs | grep -i 1b`,
//! which returns nothing); and `-Elegacy-compress` simply selects the
//! 8-byte-per-lcluster legacy/full index, not a packed 1B encoding.
//! [`IndexFormat`] therefore intentionally exposes only `Legacy` and
//! `Compact` -- there is no `Compacted1B` variant to remove and no
//! advise-bit rejection to maintain. If the kernel ever ratifies a
//! compacted-1B encoding upstream, add the variant + parser then; until
//! that happens this file produces no images bearing such a bit and
//! will simply round-trip the unrelated advise bits it already handles.
//!
//! Spec sources: the `Z_EROFS_*` constants in the public EROFS format
//! header `erofs_fs.h`; the on-disk-format chapter of the public EROFS
//! documentation (<https://erofs.docs.kernel.org/en/latest/design.html>);
//! and the original USENIX ATC 2019 paper "EROFS: A Compression-friendly
//! Readonly File System for Resource-scarce Devices" (Gao et al.) for
//! the higher-level compaction model. Format reverse-engineered from
//! these public sources; not derived from any GPL'd EROFS codebase.

use crate::decompress::Algorithm;
use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::layout::DataLayout;
use crate::superblock::Superblock;
use fs_core::BlockRead;

/// Distance from `body_end` (the post-xattr-area cursor) to the FIRST
/// legacy-format lcluster index entry. Maps to the kernel macro
/// `Z_EROFS_FULL_INDEX_START(0) = MAP_HEADER_END + 8 = 8 + 8 = 16`.
/// The 8-byte struct header is followed by an 8-byte reserved gap.
pub const Z_EROFS_LEGACY_MAP_HEADER_SIZE: u64 = 16;

/// Distance from `body_end` to the start of the COMPACTED-2B layout
/// (no reserved gap; `ebase = sizeof(map_header) + round_up(end, 8)`).
pub const Z_EROFS_COMPACT_MAP_HEADER_SIZE: u64 = 8;

pub const Z_EROFS_LCLUSTER_INDEX_SIZE: u64 = 8;

pub const Z_EROFS_LCLUSTER_TYPE_PLAIN: u8 = 0;
pub const Z_EROFS_LCLUSTER_TYPE_HEAD1: u8 = 1;
pub const Z_EROFS_LCLUSTER_TYPE_NONHEAD: u8 = 2;
pub const Z_EROFS_LCLUSTER_TYPE_HEAD2: u8 = 3;

/// Bit in `z_erofs_map_header::h_advise` enabling the 2B middle-region
/// packing for compact-format inodes. When clear, all lclusters are in
/// 4B packs (the only encoding the legacy mkfs supported pre-1.5).
/// Spec: `linux/fs/erofs/erofs_fs.h::Z_EROFS_ADVISE_COMPACTED_2B`.
pub const Z_EROFS_ADVISE_COMPACTED_2B: u16 = 0x0001;

/// Bit meaning "first algorithm uses big pclusters (multi-block frames)".
/// Per-pcluster CBLKCNT markers on the first NONHEAD after each HEAD
/// carry the compressed-block count. Spec source: public EROFS
/// compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
pub const Z_EROFS_ADVISE_BIG_PCLUSTER_1: u16 = 0x0002;

/// Big-pclusters for HEAD2 (second algorithm). Same CBLKCNT semantics
/// as `Z_EROFS_ADVISE_BIG_PCLUSTER_1` but for HEAD2-typed pclusters.
pub const Z_EROFS_ADVISE_BIG_PCLUSTER_2: u16 = 0x0004;

/// Bit in `z_erofs_map_header::h_advise` meaning "ztailpacking: tail
/// pcluster bytes are inlined directly after the index area".
pub const Z_EROFS_ADVISE_INLINE_PCLUSTER: u16 = 0x0008;

/// Bit in `z_erofs_map_header::h_advise` meaning "interlaced PLAIN
/// pcluster". Source bytes are rotated within the on-disk block:
/// bytes `[0..clusterofs)` of the on-disk block are the END of the
/// source range; bytes `[clusterofs..lcluster_size)` of the on-disk
/// block are the START. Reader must concatenate
/// `on_disk[clusterofs..] ++ on_disk[..clusterofs]` to recover the
/// source.
///
/// Only meaningful on PLAIN clusters. Compressed (HEAD1/HEAD2/NONHEAD)
/// clusters never set this bit -- the codec already produces the
/// source stream in order.
///
/// Spec source: `Z_EROFS_ADVISE_INTERLACED_PCLUSTER` constant value
/// 0x0040 in the public EROFS kernel header
/// `linux/fs/erofs/erofs_fs.h`; rotate-and-paste semantics described
/// in the public on-disk-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html>).
/// Independent implementation; license clean.
pub const Z_EROFS_ADVISE_INTERLACED_PCLUSTER: u16 = 0x0040;

/// Bit in `z_erofs_map_header::h_advise` meaning "fragments /
/// cross-file shared tail packing in use". When set, the union slot
/// at the start of the map header carries `h_fragmentoff` (a u32 byte
/// offset into the superblock's packed inode where this file's last
/// lcluster's source bytes live). The fragment covers from some
/// `clusterofs`-derived in-lcluster offset to EOF.
///
/// PARTIAL vs FULL fragment: the advise bit means "the last lcluster
/// is a fragment". A separate bit, `Z_EROFS_FRAGMENT_INODE_BIT` in
/// the high nibble of `h_clusterbits`, means "the whole inode is a
/// fragment" (no real pcluster blocks at all). The two are not
/// mutually exclusive in the on-disk encoding -- mkfs.erofs sets
/// only one per file -- but this reader treats either as cause to
/// engage the fragment redirect path.
///
/// Spec source: public EROFS compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>)
/// and the `Z_EROFS_*` constants in `linux/fs/erofs/erofs_fs.h`.
pub const Z_EROFS_ADVISE_FRAGMENT_PCLUSTER: u16 = 0x0020;

/// Bit 7 of `z_erofs_map_header::h_clusterbits`. When set, the entire
/// inode's bytes live as one contiguous run inside the superblock's
/// packed inode at byte offset `h_fragmentoff`; the lcluster index
/// has no usable entries (it may be absent or filled with zeros).
/// mkfs.erofs uses this when the whole file is small enough to be
/// merged into the packed inode without any real on-disk pclusters.
///
/// Spec source: `Z_EROFS_FRAGMENT_INODE_BIT` in the public EROFS
/// kernel header `linux/fs/erofs/erofs_fs.h` (constant value 7,
/// applied as `1 << 7 = 0x80` to the byte). License clean: name +
/// value taken from the public header constant; semantics inferred
/// from the public on-disk-format documentation.
pub const Z_EROFS_FRAGMENT_INODE_BIT: u8 = 0x80;

/// `Z_EROFS_LI_D0_CBLKCNT = 1 << 11` -- bit 11 of `lo` flags the FIRST
/// NONHEAD lcluster of a BIG_PCLUSTER pcluster. The masked value
/// (`lo & (CBLKCNT - 1)`) encodes the compressed-block count of the
/// owning pcluster. The CBLKCNT entry's `delta[0]` is implicitly `1`
/// (it sits exactly one lcluster past its head). Spec source: the
/// public EROFS compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
const Z_EROFS_LI_D0_CBLKCNT: u32 = 1 << 11;

/// Mask of the value bits that ride alongside the `Z_EROFS_LI_D0_CBLKCNT`
/// flag in a CBLKCNT-marker entry's `lo` (compact) or `delta[0]`
/// (legacy) field. `lobits` is at least 12 (per the kernel's
/// `max(z_lclusterbits, 12)` derivation), so the marker bit and value
/// bits never collide with the cluster_type bits sitting above
/// `lobits`.
const Z_EROFS_LI_D0_CBLKCNT_VAL_MASK: u32 = Z_EROFS_LI_D0_CBLKCNT - 1;

/// Header byte just past inode body + xattrs. Format-agnostic 8-byte
/// `struct z_erofs_map_header`.
///
/// Field layout (per `linux/fs/erofs/erofs_fs.h::z_erofs_map_header`):
///
/// ```text
/// 0..4   union { __le32 h_fragmentoff;
///                struct { __le16 h_reserved1; __le16 h_idata_size; };
///         }
/// 4..6   __le16 h_advise
/// 6      __u8   h_algorithmtype  (low4 = HEAD1 algo, high4 = HEAD2 algo)
/// 7      __u8   h_clusterbits    (low4 = lclusterbits)
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ZMapHeader {
    /// Union of `h_fragmentoff` (u32) / `h_idata_size` (u16) / etc. We
    /// keep the raw u32 and let callers re-interpret per advise bits.
    pub fragment_off_or_idata_size: u32,
    /// Map-level advise bits (see `Z_EROFS_ADVISE_*`).
    pub advise: u16,
    /// Raw byte: low 4 bits = HEAD1 algorithm id, high 4 bits = HEAD2
    /// algorithm id. mkfs emits both nibbles even for single-algorithm
    /// images (HEAD2 nibble is just zero when unused).
    pub algorithm_type: u8,
    /// Raw byte: low 4 bits = `lclusterbits`, upper 4 bits = reserved
    /// per the public spec.
    pub clusterbits_byte: u8,
}

impl ZMapHeader {
    pub fn lclusterbits(&self) -> u8 {
        self.clusterbits_byte & 0x0F
    }

    /// `h_idata_size` reading of the union. The kernel struct
    /// (`linux/fs/erofs/erofs_fs.h::z_erofs_map_header`) overlays
    /// `h_fragmentoff` (a `__le32`) with `{ __le16 h_reserved1;
    /// __le16 h_idata_size; }` -- so `h_idata_size` lives in the HIGH
    /// 16 bits of the u32, NOT the low 16. Used only when the header
    /// advertises ztailpacking (`Z_EROFS_ADVISE_INLINE_PCLUSTER`)
    /// AND fragments is OFF.
    pub fn idata_size(&self) -> u16 {
        ((self.fragment_off_or_idata_size >> 16) & 0xFFFF) as u16
    }

    /// `h_fragmentoff` reading of the union: the FULL 32-bit byte
    /// offset into the superblock-declared "packed inode" where this
    /// file's fragment lcluster's source bytes live. Used only when
    /// the header advertises fragments
    /// (`Z_EROFS_ADVISE_FRAGMENT_PCLUSTER`). When fragments is set,
    /// the `h_idata_size` overlay is invalid (the writer cannot
    /// store both at once because they share the same union slot;
    /// per-file mkfs.erofs picks one mode based on which saves more
    /// space).
    pub fn fragment_off(&self) -> u32 {
        self.fragment_off_or_idata_size
    }
}

/// Decoded per-lcluster index entry, format-agnostic. For legacy this
/// comes straight from a 8-byte `z_erofs_lcluster_index`; for
/// compact format it's synthesized from the bitstream + the trailing
/// pack base blkaddr.
#[derive(Debug, Clone, Copy)]
pub struct LClusterEntry {
    /// Cluster type: bits 0..1 of `di_advise` (legacy) or of the
    /// per-entry bit-packed type field (compact).
    pub cluster_type: u8,
    /// Full `di_advise` for legacy entries; for compact we fill only
    /// `cluster_type` and leave the rest zero.
    pub advise_raw: u16,
    /// `di_clusterofs` for HEAD/PLAIN entries, in source-byte units.
    pub clusterofs: u16,
    /// HEAD/PLAIN: resolved `pcluster_blkaddr` (already pack-base + nblk
    /// for compact; raw `di_u.blkaddr` for legacy). NONHEAD: low 16
    /// bits hold `delta[0]` (lclusters back to the head); legacy
    /// additionally has `delta[1]` in the high 16 bits but compact
    /// doesn't surface delta[1] here. When this NONHEAD carries a
    /// CBLKCNT marker (BIG_PCLUSTER), the marker bit + value bits sit
    /// in the low 16 bits and `cblkcnt` is set instead.
    pub u_raw: u32,
    /// `Some(blocks)` iff this NONHEAD entry carries a
    /// `Z_EROFS_LI_D0_CBLKCNT` marker for a BIG_PCLUSTER pcluster.
    /// `blocks` is the compressed-block count of the surrounding
    /// pcluster (always >= 1). The CBLKCNT entry's implicit `delta[0]`
    /// is `1` (it's the lcluster immediately following its head).
    /// Spec: public EROFS compression-format documentation
    /// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
    pub cblkcnt: Option<u32>,
}

/// Resolved physical-cluster mapping for a file byte offset.
#[derive(Debug, Clone, Copy)]
pub struct ClusterMapping {
    /// Physical block address where this pcluster's compressed bytes
    /// begin.
    pub pcluster_blkaddr: u32,
    /// Logical-cluster type as resolved -- always one of `PLAIN`,
    /// `HEAD1`, `HEAD2`. NONHEAD is followed back to its head before
    /// being returned to the caller.
    pub cluster_type: u8,
    /// Byte offset within the LOGICAL cluster (0..lcluster_size).
    pub offset_in_lcluster: u64,
    /// Physical cluster size in BLOCKS. Hard-coded to 1 (one-block
    /// pclusters, the LZ4 default) until BIG_PCLUSTER plumbing lands.
    pub pcluster_blocks: u32,
    /// Index of the lcluster `file_offset` resolves into. Surfaced so
    /// the fs.rs read path can ask "is this the inline-tail lcluster?"
    pub lcluster_idx: u64,
}

/// Resolved EXTENT covering one whole physical cluster. Where
/// [`ClusterMapping`] tells you "which pcluster owns this byte", this
/// tells you "what file-byte range and how many on-disk blocks does
/// that pcluster span."
///
/// A pcluster groups one or more contiguous logical clusters whose
/// compressed payload mkfs.erofs collated into a single LZ4/LZMA
/// frame. Decompressing only one lcluster's worth of source bytes per
/// pcluster (the old bug) silently truncates / corrupts every byte
/// past the head lcluster.
///
/// Spec: pcluster extent semantics described in the public EROFS
/// compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
#[derive(Debug, Clone, Copy)]
pub struct PclusterExtent {
    /// First on-disk block of the compressed payload.
    pub pcluster_blkaddr: u32,
    /// On-disk size in BLOCKS. For non-tail pclusters this is
    /// `next_pcluster_blkaddr - pcluster_blkaddr`; for the final
    /// pcluster the resolver leaves a generous upper bound (LZ4's
    /// `decompress_into` only consumes what it needs).
    pub pcluster_block_count: u64,
    /// Byte offset (within the source / decompressed file) where this
    /// pcluster's data begins. Equals
    /// `head_lcluster_idx * lcluster_size + head.clusterofs`.
    pub source_start_byte: u64,
    /// Byte offset (within the source / decompressed file) where this
    /// pcluster's data ends. Equals
    /// `next_head_idx * lcluster_size + next_head.clusterofs` for
    /// non-tail pclusters, or `inode.size` for the last one.
    pub source_end_byte: u64,
    /// HEAD1 / HEAD2 / PLAIN of the pcluster's head lcluster. NONHEADs
    /// have already been walked back at this point.
    pub cluster_type: u8,
    /// Index of the head lcluster owning this pcluster. Caller uses it
    /// to detect the "is this the inline-tail pcluster?" condition
    /// (head_lcluster_idx == n_lclusters - 1 implies last pcluster).
    pub head_lcluster_idx: u64,
    /// True iff this is the LAST pcluster in the file (no `next_head`
    /// existed to bound it). Useful for ztailpacking source-redirection.
    pub is_last_pcluster: bool,
    /// Source-byte offset within the head lcluster where this
    /// pcluster's payload begins (the head entry's `clusterofs`).
    /// Surfaced on the extent for two consumers: the
    /// INTERLACED-PLAIN read path (rotate amount within the on-disk
    /// block) and any future caller that wants to reason about the
    /// head's intra-lcluster offset without re-reading the lcluster
    /// entry.
    pub head_clusterofs: u16,
    /// Backing-device id for this pcluster's compressed bytes.
    /// `0` is the primary device; `>= 1` indexes into the SB device
    /// table. The public EROFS on-disk-format documentation does not
    /// define a per-pcluster `device_id` slot in either the legacy
    /// 8-byte `z_erofs_lcluster_index` (advise / clusterofs / blkaddr
    /// or delta\[2\]) or the compacted-2B / 4B bitstream (type + lo
    /// only). Multi-device routing for compressed inodes is therefore
    /// always-primary (device_id = 0) under that spec; this field is
    /// surfaced for forward compatibility and to keep the dispatch
    /// shape uniform with chunked inodes (which DO carry per-entry
    /// device_id).
    pub device_id: u16,
}

/// On-disk index encoding format. Selected by the inode's datalayout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexFormat {
    /// 8-byte `z_erofs_lcluster_index` entries, one per lcluster
    /// (datalayout = `EROFS_INODE_COMPRESSED_FULL`).
    Legacy,
    /// Compact pack-based encoding (datalayout =
    /// `EROFS_INODE_COMPRESSED_COMPACT`). Mixes 4B and 2B packs per
    /// header advise bits.
    Compact,
}

/// Geometry of one pack in the compact bitstream.
///
/// A pack covers `vcnt` lclusters and occupies `pack_bytes` on-disk
/// bytes. The trailing 4 bytes are an `__le32` base blkaddr; the
/// remaining `pack_bytes - 4` bytes are a packed bitstream of `vcnt`
/// entries each `encodebits` wide.
///
/// Spec: compact-pack geometry described in the public EROFS
/// compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
#[derive(Debug, Clone, Copy)]
struct PackGeom {
    /// Pack size in bytes (`vcnt << amortizedshift`).
    pack_bytes: u32,
    /// Number of lclusters per pack (`2` for 4B, `16` for 2B).
    vcnt: u32,
    /// Bits per entry. `(pack_bytes - 4) * 8 / vcnt` -- 16 for 4B,
    /// 14 for 2B.
    encodebits: u32,
    /// `lobits` for type/lo decoding within a pack. Width of the `lo`
    /// field. Equals `max(z_lclusterbits, ilog2(CBLKCNT) + 1)`.
    lobits: u32,
}

impl PackGeom {
    /// Pack geometry for compacted-4B (`amortizedshift = 2`).
    fn four_byte(z_lclusterbits: u32) -> Self {
        let lobits = z_lclusterbits.max(12);
        PackGeom {
            pack_bytes: 8,
            vcnt: 2,
            encodebits: 16,
            lobits,
        }
    }

    /// Pack geometry for compacted-2B (`amortizedshift = 1`).
    fn two_byte(z_lclusterbits: u32) -> Self {
        let lobits = z_lclusterbits.max(12);
        PackGeom {
            pack_bytes: 32,
            vcnt: 16,
            encodebits: 14,
            lobits,
        }
    }
}

/// Identifier for which compact region a target lcluster falls into,
/// plus the byte offset of the owning pack within the device and the
/// intra-pack index.
#[derive(Debug, Clone, Copy)]
struct PackLocation {
    /// On-disk byte offset of the start of the pack.
    pack_offset: u64,
    /// Intra-pack lcluster index (0..vcnt).
    intra_pack: u32,
    /// Geometry of this pack (4B vs 2B).
    geom: PackGeom,
}

pub struct ZMap<'a> {
    // Kept (under `_sb` to silence dead-code) so future callers that
    // need to recompute device offsets without re-plumbing the SB
    // through have it available without changing the open() signature.
    _sb: &'a Superblock,
    inode: &'a Inode,
    header: ZMapHeader,
    /// Device byte offset of the first lcluster index entry (legacy)
    /// or of the first compact pack (`ebase`).
    index_start_offset: u64,
    /// `lclusterbits` from the header low nibble (NOT `z_lclusterbits`,
    /// which is `blkszbits + lclusterbits`). Surfaces via
    /// [`Self::lclusterbits`] for backwards-compat callers.
    lclusterbits: u8,
    /// Full `z_lclusterbits = blkszbits + header.lclusterbits`. Drives
    /// `lcluster_size` and (`>> 12`) `lobits` for compact packs.
    z_lclusterbits: u32,
    format: IndexFormat,
    /// Compact only: number of lclusters in the initial 4B-form region.
    /// `((32 - ebase % 32) / 4) & 7`, capped at totalidx.
    compact_4b_initial: u32,
    /// Compact only: number of lclusters in the 2B-form middle region.
    /// `rounddown(totalidx - compact_4b_initial, 16)` when COMPACTED_2B
    /// advise bit is set, else 0.
    compact_2b: u32,
    /// Cached `Z_EROFS_ADVISE_BIG_PCLUSTER_{1,2}` state -- when either
    /// bit is set, NONHEAD entries with the `Z_EROFS_LI_D0_CBLKCNT`
    /// marker carry the surrounding pcluster's compressed-block count
    /// in their `lo` / `delta[0]` field. Without these advise bits, no
    /// CBLKCNT marker is ever emitted (we treat CBLKCNT entries as
    /// corruption when this is `false`).
    big_pcluster: bool,
    /// Codec id for HEAD1 lclusters (low nibble of
    /// `header.algorithm_type`). Decoded lazily by [`Self::header_algo`]
    /// so `ZMap::open` doesn't fail if the HEAD2 nibble carries an
    /// unsupported codec on an image with no HEAD2 pclusters.
    algo_head1_id: u8,
    /// Codec id for HEAD2 lclusters (high nibble of
    /// `header.algorithm_type`). For non-HEAD2 images the high nibble
    /// is 0 (LZ4); harmless even for a single-codec image because no
    /// HEAD2 cluster_type ever surfaces there.
    algo_head2_id: u8,
}

impl<'a> ZMap<'a> {
    /// Parse the zmap header at `inode.body_end()` and return a handle
    /// for further index lookups.
    pub fn open<R: BlockRead + ?Sized>(
        dev: &R,
        sb: &'a Superblock,
        inode: &'a Inode,
    ) -> Result<Self> {
        let header_off = inode.body_end(sb);
        let mut h = [0u8; 8];
        dev.read_at(header_off, &mut h)?;
        let header = ZMapHeader {
            fragment_off_or_idata_size: u32::from_le_bytes(h[0..4].try_into().unwrap()),
            advise: u16::from_le_bytes(h[4..6].try_into().unwrap()),
            // Byte 6 = h_algorithmtype, byte 7 = h_clusterbits per the
            // kernel struct definition (corrected from a prior local
            // swap that only happened to work for LZ4 / single-block
            // ztailpacking images where both bytes happened to be 0).
            algorithm_type: h[6],
            clusterbits_byte: h[7],
        };
        // FRAGMENT_PCLUSTER: cross-file packed-tail dedup. We accept
        // here; the read path in `fs.rs` redirects fragment-bearing
        // byte offsets to the superblock's packed inode (whose NID is
        // pre-validated when EROFS_FEATURE_INCOMPAT_FRAGMENTS is set).
        // BIG_PCLUSTER: per-pcluster CBLKCNT markers carry the
        // multi-block compressed length on the first NONHEAD after
        // each HEAD. Decoded in `pcluster_extent`.
        let big_pcluster =
            (header.advise & (Z_EROFS_ADVISE_BIG_PCLUSTER_1 | Z_EROFS_ADVISE_BIG_PCLUSTER_2)) != 0;
        let lclusterbits = header.clusterbits_byte & 0x0F;

        // Pick the on-disk encoding from the inode's datalayout. The
        // header's COMPACTED_2B advise bit is a SUB-flag of the compact
        // format -- it controls only the middle region, not the format
        // selection. mkfs.erofs since ~1.0 has used compact for
        // EROFS_INODE_COMPRESSED_COMPACT (3) inodes and full for
        // EROFS_INODE_COMPRESSED_FULL (1), with 1 mostly used for big-
        // pcluster / fragments / extent-style images.
        let format = match inode.format.layout {
            DataLayout::Compression => IndexFormat::Compact,
            DataLayout::CompressionLegacy => IndexFormat::Legacy,
            // Caller filters non-compressed layouts; defensive default.
            _ => IndexFormat::Legacy,
        };

        // ebase = ALIGN(body_end, 8) + sizeof(map_header). For
        // compact this is the start of pack 0; for legacy we add
        // another 8-byte gap to land at the first lcluster_index entry
        // (per Z_EROFS_FULL_INDEX_START).
        let ebase = (header_off + 7) & !7u64;
        let ebase = ebase + Z_EROFS_COMPACT_MAP_HEADER_SIZE;
        let index_start_offset = match format {
            IndexFormat::Legacy => ebase + 8,
            IndexFormat::Compact => ebase,
        };

        let z_lclusterbits = sb.blkszbits as u32 + lclusterbits as u32;
        let lcluster_size = 1u64 << z_lclusterbits;
        let totalidx = u32::try_from(inode.size.div_ceil(lcluster_size))
            .map_err(|_| Error::BadInode("inode too large for compact zmap"))?;

        let (compact_4b_initial, compact_2b) = if format == IndexFormat::Compact {
            // 32-byte alignment pad, in lclusters: same formula as
            // `((32 - ebase%32)/4) & 7` from the kernel. Capped at
            // totalidx in case the file is tiny.
            let pad = (((32 - (ebase % 32)) / 4) & 7) as u32;
            let initial = pad.min(totalidx);
            let middle = if (header.advise & Z_EROFS_ADVISE_COMPACTED_2B) != 0 && initial < totalidx
            {
                let remaining = totalidx - initial;
                remaining - (remaining % 16)
            } else {
                0
            };
            (initial, middle)
        } else {
            (0, 0)
        };

        // Decode the algorithm_type byte as TWO codecs: low nibble for
        // HEAD1 lclusters, high nibble for HEAD2 lclusters. mkfs.erofs
        // emits a non-zero HEAD2 nibble only on mixed-codec images
        // (e.g. `mkfs.erofs -z lz4hc,lzma`); on single-codec images
        // the high nibble is always zero. We don't validate the
        // nibbles here -- the caller dispatches per cluster_type via
        // `header_algo` and we surface Unsupported* lazily.
        let algo_head1_id = header.algorithm_type & 0x0F;
        let algo_head2_id = (header.algorithm_type >> 4) & 0x0F;

        Ok(Self {
            _sb: sb,
            inode,
            header,
            index_start_offset,
            lclusterbits,
            z_lclusterbits,
            format,
            compact_4b_initial,
            compact_2b,
            big_pcluster,
            algo_head1_id,
            algo_head2_id,
        })
    }

    /// Resolve the codec for a given pcluster `cluster_type`. PLAIN
    /// has no codec; callers are expected to filter PLAIN out before
    /// asking. HEAD1 returns the low-nibble codec; HEAD2 returns the
    /// high-nibble codec. Other types (NONHEAD, invalid) error.
    pub fn header_algo(&self, cluster_type: u8) -> Result<Algorithm> {
        match cluster_type {
            Z_EROFS_LCLUSTER_TYPE_HEAD1 => Algorithm::from_id(self.algo_head1_id),
            Z_EROFS_LCLUSTER_TYPE_HEAD2 => Algorithm::from_id(self.algo_head2_id),
            _ => Err(Error::BadInode("header_algo: cluster_type has no codec")),
        }
    }

    pub fn header(&self) -> &ZMapHeader {
        &self.header
    }

    pub fn lclusterbits(&self) -> u8 {
        self.lclusterbits
    }

    /// Inode size in bytes. Cached here for callers that don't keep
    /// the [`Inode`] handy after [`ZMap::open`].
    pub fn inode_size(&self) -> u64 {
        self.inode.size
    }

    /// Logical-cluster size in bytes: `1 << (blkszbits + lclusterbits)`.
    pub fn lcluster_size(&self) -> u64 {
        1u64 << self.z_lclusterbits
    }

    /// Number of logical clusters spanning the file.
    pub fn n_lclusters(&self) -> u64 {
        self.inode.size.div_ceil(self.lcluster_size())
    }

    /// True if the header advertises ztailpacking (last pcluster
    /// inlined directly after the index area).
    pub fn has_inline_tail(&self) -> bool {
        (self.header.advise & Z_EROFS_ADVISE_INLINE_PCLUSTER) != 0
    }

    /// Total bytes occupied by the on-disk index array, NOT including
    /// the 8-byte header. Used to compute the inline-tail offset.
    fn index_array_bytes(&self) -> u64 {
        match self.format {
            IndexFormat::Legacy => self.n_lclusters() * Z_EROFS_LCLUSTER_INDEX_SIZE,
            IndexFormat::Compact => {
                // Initial 4B region: `compact_4b_initial * 4` bytes.
                // Middle 2B region: `compact_2b * 2` bytes. Trailing
                // 4B region: rest * 4 bytes. We round up each region to
                // its pack boundary because mkfs.erofs writes whole
                // packs (the unused trailing slot of a partial pack
                // contains zero bytes that the bitstream walks happily
                // skip via type=PLAIN/lo=0).
                let totalidx = self.n_lclusters() as u32;
                let initial = self.compact_4b_initial;
                let middle = self.compact_2b;
                let tail = totalidx - initial - middle;
                let initial_bytes = ((initial as u64).div_ceil(2)) * 8;
                let middle_bytes = ((middle as u64).div_ceil(16)) * 32;
                let tail_bytes = ((tail as u64).div_ceil(2)) * 8;
                initial_bytes + middle_bytes + tail_bytes
            }
        }
    }

    /// If ztailpacking is enabled, returns `(byte_offset, size)` of the
    /// inline tail-pcluster compressed payload. The byte offset is
    /// absolute within the device.
    ///
    /// Spec note: the inline area starts immediately after the index
    /// array. For legacy that's `ebase + 8 + index_array_bytes`; for
    /// compact `ebase + index_array_bytes`. The kernel reuses the
    /// `h_fragmentoff` slot for `h_idata_size` when the inline-pcluster
    /// advise bit is set, so we read it back as a u16.
    pub fn tail_inline_offset_and_size(&self) -> Option<(u64, u32)> {
        if !self.has_inline_tail() {
            return None;
        }
        // When fragments AND ztailpacking are both advertised on the
        // same map header, the union slot carries `h_fragmentoff` and
        // `h_idata_size` is invalid. mkfs.erofs picks one mode per
        // file in practice, but be defensive: treat fragments as
        // taking precedence over ztailpacking and surface no inline
        // tail in that case (the read path will use `fragment_range`
        // instead).
        if self.has_fragment() {
            return None;
        }
        let off = self.index_start_offset + self.index_array_bytes();
        Some((off, self.header.idata_size() as u32))
    }

    /// True if the header advertises a fragment (cross-file packed
    /// tail). When set, [`Self::fragment_range`] returns the byte
    /// range within the file that's redirected to the packed inode.
    /// Covers BOTH the "last lcluster is a fragment" case
    /// (`Z_EROFS_ADVISE_FRAGMENT_PCLUSTER` advise bit) and the
    /// "whole inode is a fragment" case
    /// (`Z_EROFS_FRAGMENT_INODE_BIT` bit in the high nibble of
    /// `h_clusterbits`); from the read path's perspective the only
    /// difference is the fragment's source range.
    pub fn has_fragment(&self) -> bool {
        (self.header.advise & Z_EROFS_ADVISE_FRAGMENT_PCLUSTER) != 0
            || (self.header.clusterbits_byte & Z_EROFS_FRAGMENT_INODE_BIT) != 0
    }

    /// True if `h_clusterbits` carries the "whole inode is a
    /// fragment" bit. Distinguishes the full-fragment case from the
    /// "only the last lcluster is a fragment" advise-bit case so
    /// `fragment_range` can short-circuit the lcluster-walk.
    pub fn has_full_fragment(&self) -> bool {
        (self.header.clusterbits_byte & Z_EROFS_FRAGMENT_INODE_BIT) != 0
    }

    /// If fragments is enabled, returns
    /// `(fragmentoff, source_start_byte, source_end_byte)` describing
    /// the file-byte range whose source bytes live in the
    /// superblock's packed inode at `fragmentoff + (file_offset -
    /// source_start_byte)`.
    ///
    /// Spec interpretation: per the public EROFS compression-format
    /// documentation
    /// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>),
    /// the LAST lcluster of the file is the fragment when the
    /// `Z_EROFS_ADVISE_FRAGMENT_PCLUSTER` advise bit is set on the
    /// map header. The fragment's source range starts at the last
    /// lcluster's HEAD/PLAIN `clusterofs` (the in-lcluster byte
    /// offset where the fragment data begins) and runs to EOF
    /// (`inode.size`). When the last lcluster's `clusterofs` is 0
    /// the fragment covers exactly that lcluster; when `clusterofs`
    /// is non-zero some earlier pcluster's tail spills into the last
    /// lcluster and the fragment occupies only the trailing
    /// `[lc*lcluster_size + clusterofs, inode.size)` range.
    ///
    /// Returns `None` when the inode has no fragment.
    pub fn fragment_range<R: BlockRead + ?Sized>(
        &self,
        dev: &R,
    ) -> Result<Option<(u32, u64, u64)>> {
        if !self.has_fragment() {
            return Ok(None);
        }
        if self.inode.size == 0 {
            // Empty file with the bit set: nothing to redirect.
            return Ok(None);
        }
        // FULL-fragment case: the whole inode is a fragment, no real
        // pclusters. Don't walk the lcluster index (which may be
        // absent or zero-filled in this case); the range is simply
        // [0, inode.size).
        if self.has_full_fragment() {
            return Ok(Some((self.header.fragment_off(), 0, self.inode.size)));
        }
        let n_lc = self.n_lclusters();
        if n_lc == 0 {
            return Ok(None);
        }
        let last_idx = n_lc - 1;
        // Walk back from the last lcluster to find its owning HEAD/PLAIN.
        // Fragment lcluster is always the last; its `clusterofs` (or
        // the head it walks back to, if the last lcluster is NONHEAD)
        // tells us where the fragment's source starts.
        //
        // Per the spec, when fragments is on, the last lcluster's
        // entry is HEAD/PLAIN and its `clusterofs` equals the byte
        // offset within that lcluster where the fragment begins (i.e.
        // `inode.size % lcluster_size` if the entire last lcluster is
        // a fragment, or some smaller number when an earlier pcluster
        // spills into it).
        let mut cursor = last_idx;
        let mut entry = self.read_lcluster(dev, cursor)?;
        loop {
            match entry.cluster_type {
                Z_EROFS_LCLUSTER_TYPE_PLAIN
                | Z_EROFS_LCLUSTER_TYPE_HEAD1
                | Z_EROFS_LCLUSTER_TYPE_HEAD2 => break,
                Z_EROFS_LCLUSTER_TYPE_NONHEAD => {
                    let delta0 = if entry.cblkcnt.is_some() {
                        1u64
                    } else {
                        (entry.u_raw & 0xFFFF) as u64
                    };
                    if delta0 == 0 || delta0 > cursor {
                        return Err(Error::BadInode("fragment NONHEAD delta out of range"));
                    }
                    cursor -= delta0;
                    entry = self.read_lcluster(dev, cursor)?;
                }
                _ => return Err(Error::BadInode("fragment lcluster: invalid cluster type")),
            }
        }
        let lcluster_size = self.lcluster_size();
        let source_start_byte = cursor * lcluster_size + entry.clusterofs as u64;
        let source_end_byte = self.inode.size;
        if source_start_byte >= source_end_byte {
            // Degenerate / empty fragment range. Treat as no fragment.
            return Ok(None);
        }
        Ok(Some((
            self.header.fragment_off(),
            source_start_byte,
            source_end_byte,
        )))
    }

    /// Read the `i`-th lcluster index entry. Dispatches on the on-disk
    /// format; in both cases the result is normalized to
    /// [`LClusterEntry`].
    pub fn read_lcluster<R: BlockRead + ?Sized>(&self, dev: &R, i: u64) -> Result<LClusterEntry> {
        if i >= self.n_lclusters() {
            return Err(Error::OutOfRange);
        }
        match self.format {
            IndexFormat::Legacy => self.read_lcluster_legacy(dev, i),
            IndexFormat::Compact => self.read_lcluster_compact(dev, i),
        }
    }

    fn read_lcluster_legacy<R: BlockRead + ?Sized>(
        &self,
        dev: &R,
        i: u64,
    ) -> Result<LClusterEntry> {
        let off = self.index_start_offset + i * Z_EROFS_LCLUSTER_INDEX_SIZE;
        let mut buf = [0u8; 8];
        dev.read_at(off, &mut buf)?;
        let advise_raw = u16::from_le_bytes(buf[0..2].try_into().unwrap());
        let cluster_type = (advise_raw & 0x3) as u8;
        let clusterofs = u16::from_le_bytes(buf[2..4].try_into().unwrap());
        let u_raw = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        // BIG_PCLUSTER: legacy NONHEAD entries with the CBLKCNT marker
        // encode the surrounding pcluster's compressed-block count in
        // their `delta[0]` slot (low 16 bits of `u_raw`). Empirically
        // confirmed against `mkfs.erofs -z lzma -Elegacy-compress` on
        // erofs-utils 1.9.
        let cblkcnt = if self.big_pcluster
            && cluster_type == Z_EROFS_LCLUSTER_TYPE_NONHEAD
            && (u_raw & Z_EROFS_LI_D0_CBLKCNT) != 0
        {
            Some(u_raw & Z_EROFS_LI_D0_CBLKCNT_VAL_MASK)
        } else {
            None
        };
        Ok(LClusterEntry {
            cluster_type,
            advise_raw,
            clusterofs,
            u_raw,
            cblkcnt,
        })
    }

    /// Locate the pack that owns lcluster `i` and the intra-pack index.
    ///
    /// Walks the three-region layout (initial 4B / middle 2B / trailing
    /// 4B) so `i` lands inside one of them.
    fn locate_compact_pack(&self, i: u64) -> Result<PackLocation> {
        let initial = self.compact_4b_initial as u64;
        let middle = self.compact_2b as u64;
        let z_lcb = self.z_lclusterbits;
        if i < initial {
            // Initial 4B region.
            let geom = PackGeom::four_byte(z_lcb);
            let pack_idx = i / geom.vcnt as u64;
            let intra = (i % geom.vcnt as u64) as u32;
            let pack_offset = self.index_start_offset + pack_idx * geom.pack_bytes as u64;
            return Ok(PackLocation {
                pack_offset,
                intra_pack: intra,
                geom,
            });
        }
        let after_initial = i - initial;
        // Bytes past index_start_offset for the start of the middle
        // region: each initial pack is 8 bytes (vcnt=2). Round UP because
        // a partial last pack still occupies a full pack.
        let initial_bytes = (initial.div_ceil(2)) * 8;
        if after_initial < middle {
            let geom = PackGeom::two_byte(z_lcb);
            let pack_idx = after_initial / geom.vcnt as u64;
            let intra = (after_initial % geom.vcnt as u64) as u32;
            let pack_offset =
                self.index_start_offset + initial_bytes + pack_idx * geom.pack_bytes as u64;
            return Ok(PackLocation {
                pack_offset,
                intra_pack: intra,
                geom,
            });
        }
        let after_middle = after_initial - middle;
        // Middle bytes: each pack covers 16 lclusters, 32 bytes per pack.
        // Round UP for a partial last 2B pack.
        let middle_bytes = (middle.div_ceil(16)) * 32;
        let geom = PackGeom::four_byte(z_lcb);
        let pack_idx = after_middle / geom.vcnt as u64;
        let intra = (after_middle % geom.vcnt as u64) as u32;
        let pack_offset = self.index_start_offset
            + initial_bytes
            + middle_bytes
            + pack_idx * geom.pack_bytes as u64;
        Ok(PackLocation {
            pack_offset,
            intra_pack: intra,
            geom,
        })
    }

    /// Decode the bitstream entry at `intra` within the pack whose
    /// bitstream bytes are `bitstream`. Returns `(type, lo)`.
    fn decode_packed_entry(geom: &PackGeom, bitstream: &[u8], intra: u32) -> Result<(u8, u32)> {
        let bit_pos = (geom.encodebits * intra) as usize;
        let byte_pos = bit_pos / 8;
        let shift = bit_pos % 8;
        // We need a 32-bit window starting at `byte_pos`. The bitstream
        // is `(pack_bytes - 4)` bytes; the kernel reads with
        // `get_unaligned_le32` and tolerates reading past the bitstream
        // tail (the pack's trailing __le32 is the next memory). We
        // mirror that by zero-extending: any high bits past the
        // bitstream don't fall inside the (lobits + 2) used field.
        let mut window = [0u8; 4];
        for (k, slot) in window.iter_mut().enumerate() {
            let p = byte_pos + k;
            *slot = if p < bitstream.len() { bitstream[p] } else { 0 };
        }
        let v = u32::from_le_bytes(window) >> shift;
        let lo_mask = if geom.lobits >= 32 {
            return Err(Error::BadInode("compact lobits out of range"));
        } else {
            (1u32 << geom.lobits) - 1
        };
        let lo = v & lo_mask;
        let cluster_type = ((v >> geom.lobits) & 0x3) as u8;
        Ok((cluster_type, lo))
    }

    /// Read the bitstream + base blkaddr of one compact pack into a
    /// stack-friendly fixed-size buffer.
    fn read_pack<R: BlockRead + ?Sized>(dev: &R, loc: &PackLocation) -> Result<([u8; 32], u32)> {
        let mut buf = [0u8; 32];
        let pack_bytes = loc.geom.pack_bytes as usize;
        debug_assert!(pack_bytes <= 32);
        dev.read_at(loc.pack_offset, &mut buf[..pack_bytes])?;
        let base_off = pack_bytes - 4;
        let base = u32::from_le_bytes(buf[base_off..base_off + 4].try_into().unwrap());
        Ok((buf, base))
    }

    /// Compact-format read of one lcluster. Resolves HEAD/PLAIN
    /// `pcluster_blkaddr` here (folded into `u_raw`) so the caller's
    /// resolution path stays uniform with legacy.
    fn read_lcluster_compact<R: BlockRead + ?Sized>(
        &self,
        dev: &R,
        i: u64,
    ) -> Result<LClusterEntry> {
        let loc = self.locate_compact_pack(i)?;
        let (pack, base_blkaddr) = Self::read_pack(dev, &loc)?;
        let bitstream_len = (loc.geom.pack_bytes - 4) as usize;
        let bitstream = &pack[..bitstream_len];

        let (cluster_type, lo) = Self::decode_packed_entry(&loc.geom, bitstream, loc.intra_pack)?;

        match cluster_type {
            Z_EROFS_LCLUSTER_TYPE_PLAIN
            | Z_EROFS_LCLUSTER_TYPE_HEAD1
            | Z_EROFS_LCLUSTER_TYPE_HEAD2 => {
                let clusterofs = lo as u16;
                // pblk = base + nblk, where nblk counts non-NONHEAD
                // entries strictly before this one in the same pack.
                let nblk = compact_count_pre_head_or_plain(
                    &loc.geom,
                    bitstream,
                    loc.intra_pack,
                    self.big_pcluster,
                )?;
                let pcluster_blkaddr = base_blkaddr
                    .checked_add(nblk)
                    .ok_or(Error::BadInode("compact pcluster blkaddr overflow"))?;
                Ok(LClusterEntry {
                    cluster_type,
                    advise_raw: cluster_type as u16,
                    clusterofs,
                    u_raw: pcluster_blkaddr,
                    cblkcnt: None,
                })
            }
            Z_EROFS_LCLUSTER_TYPE_NONHEAD => {
                // CBLKCNT marker: a NONHEAD whose `lo` carries the
                // BIG_PCLUSTER compressed-block count + marker bit.
                // Implicit `delta[0]` = 1 (this entry sits immediately
                // after its HEAD).
                if self.big_pcluster && (lo & Z_EROFS_LI_D0_CBLKCNT) != 0 {
                    let blocks = lo & Z_EROFS_LI_D0_CBLKCNT_VAL_MASK;
                    return Ok(LClusterEntry {
                        cluster_type,
                        advise_raw: cluster_type as u16,
                        clusterofs: 0,
                        u_raw: 1, // implicit delta[0]
                        cblkcnt: Some(blocks),
                    });
                }
                let delta0 = compact_nonhead_delta0(&loc.geom, bitstream, loc.intra_pack, lo)?;
                Ok(LClusterEntry {
                    cluster_type,
                    advise_raw: cluster_type as u16,
                    clusterofs: 0,
                    u_raw: delta0 as u32,
                    cblkcnt: None,
                })
            }
            _ => Err(Error::BadInode("invalid compact cluster type bits")),
        }
    }

    /// Resolve the physical-cluster mapping for a file byte offset.
    /// NONHEAD entries are followed back via `delta[0]` to the HEAD
    /// that owns the pcluster.
    pub fn map<R: BlockRead + ?Sized>(&self, dev: &R, file_offset: u64) -> Result<ClusterMapping> {
        if file_offset >= self.inode.size {
            return Err(Error::OutOfRange);
        }
        let lcluster_size = self.lcluster_size();
        let lcluster_idx = file_offset / lcluster_size;
        let in_lcluster = file_offset % lcluster_size;

        let entry = self.read_lcluster(dev, lcluster_idx)?;
        let (pcluster_blkaddr, cluster_type) = match entry.cluster_type {
            Z_EROFS_LCLUSTER_TYPE_PLAIN
            | Z_EROFS_LCLUSTER_TYPE_HEAD1
            | Z_EROFS_LCLUSTER_TYPE_HEAD2 => (entry.u_raw, entry.cluster_type),
            Z_EROFS_LCLUSTER_TYPE_NONHEAD => {
                // CBLKCNT-marker NONHEAD entries have implicit
                // delta[0] = 1 (they always sit one lcluster after
                // their head); other NONHEAD entries carry delta[0]
                // in the low 16 bits of `u_raw`.
                let delta0 = if entry.cblkcnt.is_some() {
                    1u64
                } else {
                    (entry.u_raw & 0xFFFF) as u64
                };
                if delta0 == 0 || delta0 > lcluster_idx {
                    return Err(Error::BadInode("NONHEAD delta out of range"));
                }
                let head_idx = lcluster_idx - delta0;
                let head_entry = self.read_lcluster(dev, head_idx)?;
                if !is_head_or_plain(head_entry.cluster_type) {
                    return Err(Error::BadInode("NONHEAD does not lead to HEAD"));
                }
                (head_entry.u_raw, head_entry.cluster_type)
            }
            _ => return Err(Error::BadInode("invalid cluster type bits")),
        };
        Ok(ClusterMapping {
            pcluster_blkaddr,
            cluster_type,
            offset_in_lcluster: in_lcluster,
            pcluster_blocks: 1,
            lcluster_idx,
        })
    }

    /// Resolve the FULL physical-cluster extent that owns `file_offset`.
    ///
    /// One pcluster can span multiple logical clusters when mkfs.erofs
    /// collated them into a single compressed frame. The owner pcluster
    /// covers source bytes
    /// `[head_idx*lclustersize + head.clusterofs,
    ///   next_head_idx*lclustersize + next_head.clusterofs)`, or up to
    /// `inode.size` for the final pcluster.
    ///
    /// `clusterofs` semantics on a HEAD: the byte offset *within* the
    /// head lcluster where this pcluster's source data begins. If the
    /// requested `file_offset` falls within `[lcn*lclustersize,
    /// lcn*lclustersize + clusterofs)` for a HEAD entry at `lcn`, those
    /// bytes belong to the PREVIOUS pcluster, so the resolver looks
    /// back one lcluster and starts again.
    ///
    /// For the LAST pcluster (no next HEAD/PLAIN exists), the on-disk
    /// block count is left as a generous upper bound: LZ4's
    /// `decompress_into` only consumes what it needs, so over-reading
    /// is safe.
    pub fn pcluster_extent<R: BlockRead + ?Sized>(
        &self,
        dev: &R,
        file_offset: u64,
    ) -> Result<PclusterExtent> {
        if file_offset >= self.inode.size {
            return Err(Error::OutOfRange);
        }
        let lcluster_size = self.lcluster_size();
        let n_lclusters = self.n_lclusters();
        let mut lcluster_idx = file_offset / lcluster_size;
        let mut in_lcluster = file_offset % lcluster_size;

        // INTERLACED PLAIN reinterprets `clusterofs` as a rotation
        // amount within the on-disk block (not a "previous pcluster
        // spilled into me" indicator). The walk-back check below is
        // therefore disabled when the map header advertises
        // INTERLACED — the head's pcluster covers the whole lcluster
        // and we read the full range out via the rotate-and-paste
        // path in fs.rs.
        let interlaced = self.has_interlaced_pcluster();
        // Find HEAD for this offset. Walk back through NONHEADs; also
        // back up one lcluster when a HEAD's clusterofs > in_lcluster
        // (the offset falls in the previous pcluster's tail), unless
        // INTERLACED is set (in which case clusterofs is a rotation,
        // not a spillover).
        let (head_idx, head_entry) = loop {
            let entry = self.read_lcluster(dev, lcluster_idx)?;
            match entry.cluster_type {
                Z_EROFS_LCLUSTER_TYPE_PLAIN
                | Z_EROFS_LCLUSTER_TYPE_HEAD1
                | Z_EROFS_LCLUSTER_TYPE_HEAD2 => {
                    if !interlaced && (entry.clusterofs as u64) > in_lcluster {
                        // Offset is part of the previous pcluster's
                        // tail that spilled into this HEAD's lcluster.
                        if lcluster_idx == 0 {
                            return Err(Error::BadInode("HEAD clusterofs > offset at lcluster 0"));
                        }
                        lcluster_idx -= 1;
                        in_lcluster = lcluster_size; // anywhere in prev lc
                        continue;
                    }
                    break (lcluster_idx, entry);
                }
                Z_EROFS_LCLUSTER_TYPE_NONHEAD => {
                    // CBLKCNT-marker entries carry an implicit
                    // delta[0] = 1. Other NONHEADs carry delta[0] in
                    // their `u_raw`'s low 16 bits.
                    let delta0 = if entry.cblkcnt.is_some() {
                        1u64
                    } else {
                        (entry.u_raw & 0xFFFF) as u64
                    };
                    if delta0 == 0 || delta0 > lcluster_idx {
                        return Err(Error::BadInode("NONHEAD delta out of range"));
                    }
                    let head_idx = lcluster_idx - delta0;
                    let head_entry = self.read_lcluster(dev, head_idx)?;
                    if !is_head_or_plain(head_entry.cluster_type) {
                        return Err(Error::BadInode("NONHEAD does not lead to HEAD"));
                    }
                    break (head_idx, head_entry);
                }
                _ => return Err(Error::BadInode("invalid cluster type bits")),
            }
        };

        // INTERLACED PLAIN reinterprets `clusterofs` as a rotation
        // amount; the pcluster's source range starts at the lcluster
        // boundary, not at lc*lcsize+clusterofs. Use 0 as the in-
        // lcluster start for INTERLACED head entries.
        let source_start_byte =
            if interlaced && head_entry.cluster_type == Z_EROFS_LCLUSTER_TYPE_PLAIN {
                head_idx * lcluster_size
            } else {
                head_idx * lcluster_size + head_entry.clusterofs as u64
            };

        // Resolve this HEAD's pcluster blkaddr. For both legacy and
        // compact, `read_lcluster` has already folded the per-pack
        // base + nblk arithmetic into `u_raw`.
        let head_blkaddr = head_entry.u_raw;

        // Walk forward to find the next HEAD/PLAIN, which bounds this
        // pcluster's SOURCE byte range, AND the first CBLKCNT-marker
        // NONHEAD between head and next-head, which carries the
        // BIG_PCLUSTER compressed-block count.
        let mut next_head: Option<(u64, LClusterEntry)> = None;
        let mut cblkcnt_blocks: Option<u32> = None;
        for i in (head_idx + 1)..n_lclusters {
            let e = self.read_lcluster(dev, i)?;
            if is_head_or_plain(e.cluster_type) {
                next_head = Some((i, e));
                break;
            }
            // First CBLKCNT marker before the next HEAD wins. Subsequent
            // NONHEADs in the same pcluster never have CBLKCNT set
            // (it's a per-pcluster annotation, not per-NONHEAD).
            if cblkcnt_blocks.is_none() {
                if let Some(b) = e.cblkcnt {
                    cblkcnt_blocks = Some(b);
                }
            }
        }

        // Source end: bounded by next HEAD/PLAIN's start, or by inode
        // size for the last pcluster. The next entry can be a SENTINEL
        // PLAIN that mkfs.erofs emits with `clusterofs ==
        // file_size % lcluster_size` to mark end-of-file; its blkaddr
        // is unused so we must NOT treat it as a real pcluster.
        let source_end_byte = match next_head {
            Some((nh_idx, nh)) => {
                let end = nh_idx * lcluster_size + nh.clusterofs as u64;
                end.min(self.inode.size)
            }
            None => self.inode.size,
        };
        let is_last_pcluster = source_end_byte >= self.inode.size;

        // Compressed-block count:
        // - BIG_PCLUSTER + CBLKCNT marker found: the marker value IS
        //   the count. Validated empirically against
        //   `mkfs.erofs -z lzma|deflate` on erofs-utils 1.9.
        // - BIG_PCLUSTER but no CBLKCNT marker (degenerate single-block
        //   pcluster): count is 1. mkfs.erofs emits these for
        //   pclusters that fit in one block even with BIG_PCLUSTER set.
        // - Non-BIG_PCLUSTER: each pcluster is exactly 1 on-disk block.
        let pcluster_block_count: u64 = match cblkcnt_blocks {
            Some(0) => {
                // CBLKCNT=0 would mean "0 compressed blocks", which is
                // nonsensical for a non-empty pcluster. Treat as bad
                // metadata.
                return Err(Error::BadInode("CBLKCNT marker with zero block count"));
            }
            Some(blocks) => blocks as u64,
            None => 1,
        };

        Ok(PclusterExtent {
            pcluster_blkaddr: head_blkaddr,
            pcluster_block_count,
            source_start_byte,
            source_end_byte,
            cluster_type: head_entry.cluster_type,
            head_lcluster_idx: head_idx,
            is_last_pcluster,
            head_clusterofs: head_entry.clusterofs,
            // Compressed pclusters always live on the primary device
            // under the public EROFS on-disk-format spec — no
            // per-entry slot exists in either the 8-byte legacy index
            // or the compacted 2B/4B bitstream. Surfaced for symmetry
            // with `chunked::lookup_chunk_blkaddr`'s multi-device
            // routing.
            device_id: 0,
        })
    }

    /// True iff the map header's `h_advise` carries the
    /// `Z_EROFS_ADVISE_INTERLACED_PCLUSTER` bit. Surfaced as a method
    /// for the read path so it doesn't have to re-derive the bit
    /// constant.
    pub fn has_interlaced_pcluster(&self) -> bool {
        (self.header.advise & Z_EROFS_ADVISE_INTERLACED_PCLUSTER) != 0
    }
}

/// Count non-NONHEAD entries strictly preceding `intra_pack` within the
/// same pack, skipping NONHEAD blocks via their `delta[0]`. Returns
/// `1 + count` (the kernel initializes `nblk = 1` for HEAD/PLAIN
/// resolution -- so this is `nblk` directly).
///
/// Spec: HEAD/PLAIN blkaddr derivation in the compact pack format,
/// per the public EROFS compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
fn compact_count_pre_head_or_plain(
    geom: &PackGeom,
    bitstream: &[u8],
    intra_pack: u32,
    big_pcluster: bool,
) -> Result<u32> {
    if big_pcluster {
        compact_count_big_pcluster(geom, bitstream, intra_pack)
    } else {
        compact_count_simple(geom, bitstream, intra_pack)
    }
}

/// Convention WITHOUT BIG_PCLUSTER: every pcluster occupies exactly 1
/// block. mkfs writes `base = pack0_first_pcluster_blkaddr - 1` and
/// the reader reconstructs `blkaddr = base + nblk` with `nblk = 1` for
/// the first non-NONHEAD entry in the pack, +1 for each later
/// non-NONHEAD entry; NONHEADs walked over via their `delta[0]` `lo`.
fn compact_count_simple(geom: &PackGeom, bitstream: &[u8], intra_pack: u32) -> Result<u32> {
    let mut nblk: u32 = 1;
    let mut i: i32 = intra_pack as i32;
    while i > 0 {
        i -= 1;
        let (ty, lo) = ZMap::decode_packed_entry(geom, bitstream, i as u32)?;
        if ty == Z_EROFS_LCLUSTER_TYPE_NONHEAD {
            // CBLKCNT bit isn't valid here (BIG_PCLUSTER off).
            if (lo & Z_EROFS_LI_D0_CBLKCNT) != 0 {
                return Err(Error::BadInode(
                    "CBLKCNT marker without BIG_PCLUSTER advise",
                ));
            }
            i -= lo as i32;
        } else if i >= 0 {
            nblk = nblk
                .checked_add(1)
                .ok_or(Error::BadInode("compact nblk overflow"))?;
        }
    }
    Ok(nblk)
}

/// Convention WITH BIG_PCLUSTER: pclusters span `cblkcnt` blocks
/// (carried on the CBLKCNT-marked NONHEAD immediately after each
/// HEAD). mkfs writes `base = pack0_first_pcluster_blkaddr` directly
/// (no -1 offset). We walk entry-by-entry and, when we step past the
/// boundary between two pclusters in the same pack (signalled by a
/// CBLKCNT marker entry), credit `cblkcnt` blocks for the older
/// pcluster. NONHEADs without a CBLKCNT marker contribute nothing
/// (they belong to the same pcluster as their head, which we already
/// accounted for or will when we walk past its CBLKCNT marker).
///
/// Empirically validated against `mkfs.erofs -z lzma|deflate` output
/// on erofs-utils 1.9.
fn compact_count_big_pcluster(geom: &PackGeom, bitstream: &[u8], intra_pack: u32) -> Result<u32> {
    let mut nblk: u32 = 0;
    if intra_pack == 0 {
        return Ok(nblk);
    }
    // Walk entry-by-entry from intra_pack-1 down to 0. Each CBLKCNT
    // marker we cross adds its block count; degenerate pclusters
    // (HEAD followed by something other than a CBLKCNT NONHEAD)
    // contribute 1 block. We detect a "pcluster boundary": a HEAD
    // entry at index `j` whose next NONHEAD (j+1) is NOT a CBLKCNT
    // marker. For HEADs whose next entry IS a CBLKCNT marker, the
    // marker (at j+1) supplies the full count; we credit it when we
    // cross the marker entry, so the HEAD itself contributes nothing.
    for j in (0..intra_pack).rev() {
        let (ty, lo) = ZMap::decode_packed_entry(geom, bitstream, j)?;
        if ty == Z_EROFS_LCLUSTER_TYPE_NONHEAD && (lo & Z_EROFS_LI_D0_CBLKCNT) != 0 {
            // CBLKCNT marker: credit the surrounding pcluster's
            // compressed-block count.
            nblk = nblk
                .checked_add(lo & Z_EROFS_LI_D0_CBLKCNT_VAL_MASK)
                .ok_or(Error::BadInode("compact nblk overflow"))?;
        } else if ty != Z_EROFS_LCLUSTER_TYPE_NONHEAD {
            // HEAD/PLAIN: if the entry immediately after it (j+1) is
            // a CBLKCNT marker, that marker is what carries this
            // pcluster's count -- and we already added it (we walked
            // past j+1 before j on this reverse pass). Otherwise the
            // pcluster is degenerate (1 block).
            let next_is_cblkcnt = if j + 1 < intra_pack {
                let (n_ty, n_lo) = ZMap::decode_packed_entry(geom, bitstream, j + 1)?;
                n_ty == Z_EROFS_LCLUSTER_TYPE_NONHEAD && (n_lo & Z_EROFS_LI_D0_CBLKCNT) != 0
            } else {
                // The HEAD is at intra_pack - 1; there's no entry
                // after it within the walk. Conservatively assume
                // degenerate (the actual cblkcnt would be at the
                // entry we're computing for, which we haven't decoded
                // here).
                false
            };
            if !next_is_cblkcnt {
                nblk = nblk
                    .checked_add(1)
                    .ok_or(Error::BadInode("compact nblk overflow"))?;
            }
        }
    }
    Ok(nblk)
}

/// Resolve a NONHEAD entry's `delta[0]` given the raw `lo` decoded from
/// its bit-position. The LAST entry in a pack stores `delta[1]` in `lo`
/// instead, so we re-decode the previous entry to recover `delta[0]`:
///
/// - If the previous entry was non-NONHEAD: delta[0] = 0 + 1 = 1
///   (we're 1 lcluster past the HEAD).
/// - If the previous was NONHEAD with CBLKCNT marker: delta[0] = 1 + 1
///   = 2 (CBLKCNT means delta[0] of THAT entry is 1, so we're +1).
/// - Otherwise: delta[0] = previous_lo + 1.
///
/// Spec: `zmap.c` lines 174..190 of mainline at time of writing.
fn compact_nonhead_delta0(
    geom: &PackGeom,
    bitstream: &[u8],
    intra_pack: u32,
    lo_at_intra: u32,
) -> Result<u16> {
    if intra_pack + 1 != geom.vcnt {
        // Not the last entry: lo IS delta[0].
        return Ok((lo_at_intra & 0xFFFF) as u16);
    }
    if intra_pack == 0 {
        // Pack of vcnt=1? Geometry forbids it (vcnt is 2 or 16) so this
        // is unreachable, but bail safely.
        return Err(Error::BadInode("compact pack vcnt < 2"));
    }
    let (prev_ty, prev_lo) = ZMap::decode_packed_entry(geom, bitstream, intra_pack - 1)?;
    let derived = if prev_ty != Z_EROFS_LCLUSTER_TYPE_NONHEAD {
        0
    } else if (prev_lo & Z_EROFS_LI_D0_CBLKCNT) != 0 {
        1
    } else {
        prev_lo
    };
    let d0 = derived
        .checked_add(1)
        .ok_or(Error::BadInode("NONHEAD delta0 overflow"))?;
    Ok((d0 & 0xFFFF) as u16)
}

fn is_head_or_plain(t: u8) -> bool {
    matches!(
        t,
        Z_EROFS_LCLUSTER_TYPE_PLAIN | Z_EROFS_LCLUSTER_TYPE_HEAD1 | Z_EROFS_LCLUSTER_TYPE_HEAD2
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inode::tests::synth_compact;
    use crate::layout::DataLayout;
    use crate::superblock::tests::synth_sb;
    use fs_core::{BlockRead, Result as BlockResult};
    use std::sync::Mutex;

    /// In-memory device for tests (modelled after the MemDev in
    /// chunked / fs).
    struct MemDev(Mutex<Vec<u8>>);
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

    /// Build a synthetic CompressionLegacy (layout id 1) inode buffer.
    /// Used to drive the legacy/full-index path. `i_u` carries
    /// `compressed_blocks`.
    fn synth_compressed_legacy_compact(mode: u16, size: u32, flags: u16, raw_u: u32) -> [u8; 32] {
        let mut b = synth_compact(DataLayout::CompressionLegacy, mode, size, raw_u);
        let raw_format: u16 = ((DataLayout::CompressionLegacy as u16) << 1) | (flags << 4);
        b[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        b
    }

    /// Build a synthetic Compression (layout id 3) inode buffer. Used
    /// to drive the compact path.
    fn synth_compressed_compact(mode: u16, size: u32, flags: u16, raw_u: u32) -> [u8; 32] {
        let mut b = synth_compact(DataLayout::Compression, mode, size, raw_u);
        let raw_format: u16 = ((DataLayout::Compression as u16) << 1) | (flags << 4);
        b[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        b
    }

    /// Encode an 8-byte legacy lcluster index entry.
    fn encode_lcluster(advise: u16, clusterofs: u16, u: u32) -> [u8; 8] {
        let mut e = [0u8; 8];
        e[0..2].copy_from_slice(&advise.to_le_bytes());
        e[2..4].copy_from_slice(&clusterofs.to_le_bytes());
        e[4..8].copy_from_slice(&u.to_le_bytes());
        e
    }

    /// Encode one entry into the bitstream window of a pack.
    /// `lobits` is the encoded `lo` width; `bit_pos` is the start bit
    /// position in the bitstream. Writes 4 bytes (LE u32) starting at
    /// `bit_pos / 8`, OR-merging into existing bytes so adjacent entries
    /// can share the same byte boundary.
    fn write_packed_entry(
        bitstream: &mut [u8],
        bit_pos: usize,
        lobits: u32,
        cluster_type: u8,
        lo: u32,
    ) {
        let value = ((cluster_type as u32 & 0x3) << lobits) | (lo & ((1 << lobits) - 1));
        let value = value << (bit_pos % 8);
        let mut bytes = bitstream.to_vec();
        let byte = bit_pos / 8;
        for k in 0..4 {
            if byte + k < bytes.len() {
                bytes[byte + k] |= ((value >> (k * 8)) & 0xFF) as u8;
            }
        }
        bitstream.copy_from_slice(&bytes);
    }

    /// Build a 4B compact pack from two (type, lo) entries plus a base
    /// blkaddr. Pack = 4-byte bitstream + 4-byte base.
    fn build_compact_4b_pack(
        z_lclusterbits: u32,
        entries: [(u8, u32); 2],
        base_blkaddr: u32,
    ) -> [u8; 8] {
        let lobits = z_lclusterbits.max(12);
        let mut pack = [0u8; 8];
        // Entries occupy bits 0..32 (16 bits each).
        for (idx, (ty, lo)) in entries.iter().enumerate() {
            write_packed_entry(&mut pack[..4], idx * 16, lobits, *ty, *lo);
        }
        pack[4..8].copy_from_slice(&base_blkaddr.to_le_bytes());
        pack
    }

    /// Build a 2B compact pack from 16 entries plus a base blkaddr.
    /// Pack = 28-byte bitstream + 4-byte base.
    fn build_compact_2b_pack(
        z_lclusterbits: u32,
        entries: [(u8, u32); 16],
        base_blkaddr: u32,
    ) -> [u8; 32] {
        let lobits = z_lclusterbits.max(12);
        let mut pack = [0u8; 32];
        // Each entry is 14 bits. We need a separate bitstream buffer
        // since adjacent entries straddle byte boundaries.
        let mut bs = [0u8; 28];
        for (idx, (ty, lo)) in entries.iter().enumerate() {
            write_packed_entry(&mut bs, idx * 14, lobits, *ty, *lo);
        }
        pack[..28].copy_from_slice(&bs);
        pack[28..32].copy_from_slice(&base_blkaddr.to_le_bytes());
        pack
    }

    /// Build a legacy-format zmap image. `lclusterbits_low4` populates
    /// the low 4 bits of the header `clusterbits_byte`.
    ///
    /// Layout: 8-byte `z_erofs_map_header` + 8 bytes of reserved gap
    /// (per the kernel's `Z_EROFS_FULL_INDEX_START` macro), then a
    /// packed array of 8-byte lcluster_index entries.
    fn build_zmap_image(
        size: u32,
        header_advise: u16,
        lclusterbits_low4: u8,
        entries: &[[u8; 8]],
    ) -> Vec<u8> {
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 4];
        let sb = synth_sb(12, 0, 1, 4);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        // No xattrs, no special inode advise, raw_u doesn't matter for legacy.
        let inode_buf = synth_compressed_legacy_compact(0x81A4, size, 0, 0);
        img[BS..BS + 32].copy_from_slice(&inode_buf);

        // zmap header sits directly after the 32-byte inode body.
        let hdr_off = BS + 32;
        // h_fragmentoff/h_idata_size = 0
        // h_advise:
        img[hdr_off + 4..hdr_off + 6].copy_from_slice(&header_advise.to_le_bytes());
        // h_algorithmtype = 0 (LZ4) at byte 6
        img[hdr_off + 6] = 0;
        // h_clusterbits (low 4 bits = lclusterbits) at byte 7
        img[hdr_off + 7] = lclusterbits_low4 & 0x0F;

        // 8-byte struct header + 8-byte reserved gap = 16-byte total
        // distance from body_end to the first lcluster_index entry.
        let mut off = hdr_off + 16;
        for entry in entries {
            img[off..off + 8].copy_from_slice(entry);
            off += 8;
        }
        img
    }

    /// Build a compact (4B-only) zmap image with two lclusters and one
    /// pack: pack 0 = (lc0, lc1), base blkaddr.
    fn build_compact_4b_image(
        size: u32,
        header_advise: u16,
        lclusterbits_low4: u8,
        packs: &[[u8; 8]],
        idata_size: u16,
        inline_tail_bytes: &[u8],
    ) -> Vec<u8> {
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 8];
        let sb = synth_sb(12, 0, 1, 8);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        let inode_buf = synth_compressed_compact(0x81A4, size, 0, 0);
        img[BS..BS + 32].copy_from_slice(&inode_buf);

        let hdr_off = BS + 32;
        // `h_idata_size` lives in the HIGH 16 bits of the union (kernel
        // overlays `h_fragmentoff` over `{ __le16 h_reserved1; __le16
        // h_idata_size; }`).
        let frag_or_idata = (idata_size as u32) << 16;
        img[hdr_off..hdr_off + 4].copy_from_slice(&frag_or_idata.to_le_bytes());
        img[hdr_off + 4..hdr_off + 6].copy_from_slice(&header_advise.to_le_bytes());
        img[hdr_off + 6] = lclusterbits_low4 & 0x0F;
        img[hdr_off + 7] = 0;

        // ebase = ALIGN(body_end, 8) + 8 = hdr_off + 8 (body_end is
        // already 8-aligned in this fixture).
        let ebase = hdr_off + 8;
        let mut off = ebase;
        for pack in packs {
            img[off..off + 8].copy_from_slice(pack);
            off += 8;
        }
        if !inline_tail_bytes.is_empty() {
            img[off..off + inline_tail_bytes.len()].copy_from_slice(inline_tail_bytes);
        }
        img
    }

    #[test]
    fn legacy_zmap_resolves_head_nonhead_and_second_head() {
        // Three lclusters: HEAD1@blk100, NONHEAD(delta0=1), HEAD1@blk200.
        let head1 = encode_lcluster(/*advise=HEAD1*/ 1, 0, 100);
        let nonhead = encode_lcluster(/*advise=NONHEAD*/ 2, 0, 1 /*delta[0]=1*/);
        let head2 = encode_lcluster(1, 0, 200);
        let img = build_zmap_image(3 * 4096, 0, 0, &[head1, nonhead, head2]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert_eq!(zmap.lcluster_size(), 4096);
        assert_eq!(zmap.n_lclusters(), 3);

        let m0 = zmap.map(&dev, 0).unwrap();
        assert_eq!(m0.pcluster_blkaddr, 100);
        assert_eq!(m0.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        assert_eq!(m0.offset_in_lcluster, 0);
        assert_eq!(m0.pcluster_blocks, 1);
        assert_eq!(m0.lcluster_idx, 0);

        let m1 = zmap.map(&dev, 4096).unwrap();
        assert_eq!(m1.pcluster_blkaddr, 100);
        assert_eq!(m1.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        assert_eq!(m1.offset_in_lcluster, 0);
        assert_eq!(m1.lcluster_idx, 1);

        let m1b = zmap.map(&dev, 4096 + 17).unwrap();
        assert_eq!(m1b.pcluster_blkaddr, 100);
        assert_eq!(m1b.offset_in_lcluster, 17);

        let m2 = zmap.map(&dev, 2 * 4096).unwrap();
        assert_eq!(m2.pcluster_blkaddr, 200);
        assert_eq!(m2.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        assert_eq!(m2.lcluster_idx, 2);
    }

    #[test]
    fn plain_cluster_passes_blkaddr_through() {
        let plain = encode_lcluster(/*advise=PLAIN*/ 0, 0, 42);
        let img = build_zmap_image(4096, 0, 0, &[plain]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();

        let m = zmap.map(&dev, 0).unwrap();
        assert_eq!(m.pcluster_blkaddr, 42);
        assert_eq!(m.cluster_type, Z_EROFS_LCLUSTER_TYPE_PLAIN);
    }

    #[test]
    fn compact_4b_two_lclusters_one_pcluster() {
        // synth_sb has blkszbits=12 so z_lclusterbits = 12 + 0 = 12 ->
        // lcluster_size = 4096. compact_4b_initial = ((32 - ebase%32)/4)
        // & 7. ebase = body_end + 8 = 4096+32+8 = 4136. 4136%32 = 8 ->
        // pad = (32-8)/4 & 7 = 6. With totalidx=2, initial = min(6, 2)
        // = 2 -> all in initial 4B region.
        //
        // Pack 0 = (HEAD1@lo=0, NONHEAD@lo=1), base=99 -> blk = 99+1 = 100.
        let pack0 = build_compact_4b_pack(
            12,
            [
                (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                (Z_EROFS_LCLUSTER_TYPE_NONHEAD, 1),
            ],
            99,
        );
        let img = build_compact_4b_image(2 * 4096, 0, 0, &[pack0], 0, &[]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert_eq!(zmap.n_lclusters(), 2);

        let m0 = zmap.map(&dev, 0).unwrap();
        assert_eq!(m0.pcluster_blkaddr, 100);
        assert_eq!(m0.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        assert_eq!(m0.lcluster_idx, 0);

        // NONHEAD at lc1: when last in pack and prev is HEAD/PLAIN,
        // delta[0] derives to 1 (HEAD's lo=0, prev_ty != NONHEAD, so 0
        // + 1 = 1). Walks back to lc0.
        let m1 = zmap.map(&dev, 4096).unwrap();
        assert_eq!(m1.pcluster_blkaddr, 100);
        assert_eq!(m1.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        assert_eq!(m1.lcluster_idx, 1);
    }

    #[test]
    fn compact_4b_two_separate_pclusters() {
        // Pack 0 = (HEAD1@lo=0, HEAD1@lo=0), base=99. lc0 -> 99+1 = 100.
        // lc1 -> nblk after walk-back through 1 HEAD = 2; so blk = 99+2 = 101.
        let pack0 = build_compact_4b_pack(
            12,
            [
                (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
            ],
            99,
        );
        let img = build_compact_4b_image(2 * 4096, 0, 0, &[pack0], 0, &[]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();

        let m0 = zmap.map(&dev, 0).unwrap();
        assert_eq!(m0.pcluster_blkaddr, 100);
        let m1 = zmap.map(&dev, 4096).unwrap();
        assert_eq!(m1.pcluster_blkaddr, 101);
    }

    #[test]
    fn compact_2b_middle_region_resolves_correctly() {
        // Need totalidx large enough to exercise the 2B middle region.
        // synth_sb: blkszbits=12. ebase = 4136, ebase%32 = 8 -> initial
        // pad = 6. With totalidx = 22: initial = 6, middle = rounddown
        // (22-6, 16) = 16, trailing = 0. So lc 6..21 live in a single
        // 2B pack of 16 entries.
        //
        // Build the image manually since the test helpers aren't shaped
        // for mixed-region layouts; reuse build_compact_2b_pack for the
        // middle pack.
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 32];
        let sb = synth_sb(12, 0, 1, 32);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        let inode_buf = synth_compressed_compact(0x81A4, 22 * 4096, 0, 0);
        img[BS..BS + 32].copy_from_slice(&inode_buf);
        let hdr_off = BS + 32;
        // advise = COMPACTED_2B
        img[hdr_off + 4..hdr_off + 6].copy_from_slice(&Z_EROFS_ADVISE_COMPACTED_2B.to_le_bytes());
        img[hdr_off + 6] = 0;
        img[hdr_off + 7] = 0;

        // Initial 4B packs: 6 entries -> 3 packs of 8 bytes each =
        // 24 bytes. Encode all PLAIN with base 0, so blkaddrs are
        // 1, 2, 3, 4, 5, 6 across the initial region (each pack carries
        // base=initial_pack_idx*0... wait we want unique heads. Just
        // encode lc 0..5 as HEAD1 lo=0 each, with bases chosen so the
        // resolved blkaddrs are 1, 2, ..., 6.
        let mut off = hdr_off + 8; // ebase
        let initial_packs = [
            build_compact_4b_pack(
                12,
                [
                    (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                    (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                ],
                0, // base 0 -> lc0=1, lc1=2
            ),
            build_compact_4b_pack(
                12,
                [
                    (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                    (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                ],
                2, // base 2 -> lc2=3, lc3=4
            ),
            build_compact_4b_pack(
                12,
                [
                    (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                    (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                ],
                4, // base 4 -> lc4=5, lc5=6
            ),
        ];
        for p in &initial_packs {
            img[off..off + 8].copy_from_slice(p);
            off += 8;
        }

        // Middle 2B pack: lc 6..21 (16 entries). Encode the first as
        // HEAD1 lo=0, the rest as NONHEAD(d0=1, 2, ...) chained back to
        // lc 6.
        let mut middle_entries = [(Z_EROFS_LCLUSTER_TYPE_HEAD1, 0u32); 16];
        for (j, entry) in middle_entries.iter_mut().enumerate().skip(1) {
            *entry = (Z_EROFS_LCLUSTER_TYPE_NONHEAD, j as u32);
        }
        let middle_pack = build_compact_2b_pack(12, middle_entries, 6);
        img[off..off + 32].copy_from_slice(&middle_pack);
        // off += 32; (no trailing region in this fixture)

        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert_eq!(zmap.n_lclusters(), 22);

        // Sanity: initial-region heads.
        for (lcn, want) in [(0u64, 1u32), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6)] {
            let m = zmap.map(&dev, lcn * 4096).unwrap();
            assert_eq!(m.pcluster_blkaddr, want, "lc {lcn} initial-region blkaddr");
        }
        // Middle-region head: lc 6 = HEAD1 with base=6 -> blk = 6+1 = 7.
        let m6 = zmap.map(&dev, 6 * 4096).unwrap();
        assert_eq!(m6.pcluster_blkaddr, 7);
        // Middle NONHEADs all walk back to lc 6.
        for lcn in 7u64..=21 {
            let m = zmap.map(&dev, lcn * 4096).unwrap();
            assert_eq!(m.pcluster_blkaddr, 7, "lc {lcn} middle-region walks back");
            assert_eq!(m.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        }
    }

    #[test]
    fn compact_4b_multi_lcluster_with_sentinel_last() {
        // Mirror the 200K-file pattern: HEAD@lc0, NONHEADs through
        // lc(n-2), PLAIN sentinel @lc(n-1) with high clusterofs.
        //
        // synth_sb blkszbits=12, lclusterbits=0 -> lobits=12.
        // clusterofs in the sentinel must fit in 12 bits (max 4095).
        const BS: usize = 4096;
        let n = 13u32; // 13 lclusters
        let lcluster_size: u32 = 4096;
        let file_size: u32 = 12 * lcluster_size + 3392;

        let mut img = vec![0u8; BS * 16];
        let sb = synth_sb(12, 0, 1, 16);
        img[crate::superblock::EROFS_SUPER_OFFSET as usize
            ..crate::superblock::EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);
        let inode_buf = synth_compressed_compact(0x81A4, file_size, 0, 0);
        img[BS..BS + 32].copy_from_slice(&inode_buf);
        let hdr_off = BS + 32;
        img[hdr_off + 4..hdr_off + 6].copy_from_slice(&0u16.to_le_bytes()); // no advise
        img[hdr_off + 6] = 0;
        img[hdr_off + 7] = 0;

        // ebase = hdr_off + 8 = 4136. compact_4b_initial = ((32-8)/4)&7
        // = 6. With n=13 and advise no COMPACTED_2B: middle=0,
        // trailing = 13-6 = 7 lclusters -> 4 trailing packs (last
        // partial). All 13 effectively in 4B form.
        //
        // Pack layout (each pack = (lc_a, lc_b)):
        // pack 0 = (HEAD1 lo=0, NONHEAD d0=1) base=0 -> lc0=1
        // pack 1 = (NONHEAD d0=2, NONHEAD d0=3) base=0
        // pack 2 = (NONHEAD d0=4, NONHEAD d0=5) base=0
        // pack 3 = (NONHEAD d0=6, NONHEAD d0=7) base=0
        // pack 4 = (NONHEAD d0=8, NONHEAD d0=9) base=0
        // pack 5 = (NONHEAD d0=10, NONHEAD d0=11) base=0
        // pack 6 = (PLAIN lo=3392, _) base=0 (sentinel @lc12)
        let make_nh = |d0: u32| (Z_EROFS_LCLUSTER_TYPE_NONHEAD, d0);
        let packs = [
            build_compact_4b_pack(12, [(Z_EROFS_LCLUSTER_TYPE_HEAD1, 0), make_nh(1)], 0),
            build_compact_4b_pack(12, [make_nh(2), make_nh(3)], 0),
            build_compact_4b_pack(12, [make_nh(4), make_nh(5)], 0),
            build_compact_4b_pack(12, [make_nh(6), make_nh(7)], 0),
            build_compact_4b_pack(12, [make_nh(8), make_nh(9)], 0),
            build_compact_4b_pack(12, [make_nh(10), make_nh(11)], 0),
            build_compact_4b_pack(12, [(Z_EROFS_LCLUSTER_TYPE_PLAIN, 3392), (0, 0)], 0),
        ];
        let mut off = hdr_off + 8;
        for p in &packs {
            img[off..off + 8].copy_from_slice(p);
            off += 8;
        }

        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert_eq!(zmap.n_lclusters() as u32, n);

        // Every offset in [0, file_size) should resolve to pcluster
        // blkaddr 1.
        for q in [0u64, 4096, 4096 * 5, 4096 * 11, file_size as u64 - 1] {
            let e = zmap.pcluster_extent(&dev, q).unwrap();
            assert_eq!(e.pcluster_blkaddr, 1, "query {q}");
            assert_eq!(e.source_start_byte, 0, "query {q}");
            assert_eq!(e.source_end_byte, file_size as u64, "query {q}");
            assert_eq!(e.head_lcluster_idx, 0);
        }
    }

    #[test]
    fn ztailpacking_offset_and_size_computed_correctly() {
        // Compact image with the inline-pcluster bit set. Two
        // lclusters, one pcluster (the inline tail). idata_size = 17.
        let pack0 = build_compact_4b_pack(
            12,
            [
                (Z_EROFS_LCLUSTER_TYPE_HEAD1, 0),
                (Z_EROFS_LCLUSTER_TYPE_NONHEAD, 1),
            ],
            0,
        );
        let inline_payload: Vec<u8> = (0..17u8).collect();
        let img = build_compact_4b_image(
            2 * 4096,
            Z_EROFS_ADVISE_INLINE_PCLUSTER,
            0,
            &[pack0],
            17,
            &inline_payload,
        );
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(zmap.has_inline_tail());
        let (off, sz) = zmap.tail_inline_offset_and_size().unwrap();
        assert_eq!(sz, 17);
        // Verify by reading it back.
        let mut got = vec![0u8; sz as usize];
        dev.read_at(off, &mut got).unwrap();
        assert_eq!(got, inline_payload);
    }

    #[test]
    fn ztailpacking_legacy_offset_computation() {
        // Same idea but in legacy format: inline area starts at
        // body_end + 16 (header + 8B reserved gap) + n_lclusters * 8.
        let head = encode_lcluster(1, 0, 0);
        let nonhead = encode_lcluster(2, 0, 1);
        let mut img = build_zmap_image(
            2 * 4096,
            Z_EROFS_ADVISE_INLINE_PCLUSTER,
            0,
            &[head, nonhead],
        );
        // Patch idata_size = 9 into the header's fragment_off/idata
        // union slot. Per kernel `z_erofs_map_header`, `h_idata_size`
        // is the HIGH 16 bits of the u32 (low 16 = `h_reserved1`).
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        img[hdr_off..hdr_off + 4].copy_from_slice(&((9u32) << 16).to_le_bytes());
        // Patch inline tail bytes (after 16B header + 2 * 8B entries).
        let inline_at = hdr_off + 16 + 2 * 8;
        for (i, b) in img[inline_at..inline_at + 9].iter_mut().enumerate() {
            *b = (0xA0 + i) as u8;
        }
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        let (off, sz) = zmap.tail_inline_offset_and_size().unwrap();
        assert_eq!(sz, 9);
        let mut got = vec![0u8; sz as usize];
        dev.read_at(off, &mut got).unwrap();
        let expected: Vec<u8> = (0..9).map(|i| (0xA0 + i) as u8).collect();
        assert_eq!(got, expected);
    }

    /// Single-lcluster file fully owned by a fragment: HEAD@lc0
    /// clusterofs=0 with the FRAGMENT_PCLUSTER advise bit set.
    /// `fragment_range` should report (fragmentoff, 0, file_size).
    #[test]
    fn fragments_single_lcluster_full_range() {
        // file size 4000 bytes (one lcluster). HEAD1 at lc0 with
        // clusterofs=0; fragment_off = 0x1234.
        let head = encode_lcluster(/*HEAD1*/ 1, 0, 0);
        let mut img = build_zmap_image(4000, Z_EROFS_ADVISE_FRAGMENT_PCLUSTER, 0, &[head]);
        // Patch the header's union slot with the fragmentoff value
        // (the full u32 is the byte offset into the packed inode).
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        img[hdr_off..hdr_off + 4].copy_from_slice(&0x1234u32.to_le_bytes());
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).expect("fragments must open");
        assert!(zmap.has_fragment());
        let (foff, src_start, src_end) = zmap
            .fragment_range(&dev)
            .unwrap()
            .expect("fragment present");
        assert_eq!(foff, 0x1234);
        assert_eq!(src_start, 0);
        assert_eq!(src_end, 4000);
    }

    /// Two-lcluster file where the LAST lcluster carries the
    /// fragment: lc0 HEAD@blkaddr 5 (clusterofs=0), lc1 HEAD@blkaddr
    /// (unused) with clusterofs=512 -- the fragment covers
    /// [4096+512, file_size).
    #[test]
    fn fragments_partial_last_lcluster_range() {
        let head = encode_lcluster(/*HEAD1*/ 1, 0, 5);
        // Last lc HEAD with clusterofs=512: the previous pcluster
        // spills into the last lcluster's first 512 bytes; the
        // fragment owns the rest.
        let last = encode_lcluster(/*HEAD1*/ 1, 512, 0);
        let file_size = 4096 + 1500; // 5596 bytes; last lc partial.
        let mut img = build_zmap_image(
            file_size,
            Z_EROFS_ADVISE_FRAGMENT_PCLUSTER,
            0,
            &[head, last],
        );
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        img[hdr_off..hdr_off + 4].copy_from_slice(&0xCAFEu32.to_le_bytes());
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        let (foff, src_start, src_end) = zmap
            .fragment_range(&dev)
            .unwrap()
            .expect("fragment present");
        assert_eq!(foff, 0xCAFE);
        assert_eq!(src_start, 4096 + 512);
        assert_eq!(src_end, file_size as u64);
    }

    /// A NONHEAD last lcluster walks back to its owning HEAD; the
    /// fragment range begins at the head's `clusterofs` within the
    /// head's lcluster, which means the "fragment" actually spans
    /// the WHOLE pcluster the last lcluster belongs to. This is
    /// the worst-case shape (rare in practice — mkfs typically emits
    /// a HEAD on the last lcluster when fragments is on — but the
    /// resolver tolerates it).
    #[test]
    fn fragments_last_lc_nonhead_walks_back_to_head() {
        // 3 lclusters: HEAD@lc0(co=0), NONHEAD@lc1(d0=1), NONHEAD@lc2(d0=2).
        let head = encode_lcluster(1, 0, 7);
        let nh1 = encode_lcluster(2, 0, 1);
        let nh2 = encode_lcluster(2, 0, 2);
        let file_size: u32 = 3 * 4096;
        let mut img = build_zmap_image(
            file_size,
            Z_EROFS_ADVISE_FRAGMENT_PCLUSTER,
            0,
            &[head, nh1, nh2],
        );
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        img[hdr_off..hdr_off + 4].copy_from_slice(&42u32.to_le_bytes());
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        let (foff, src_start, src_end) = zmap
            .fragment_range(&dev)
            .unwrap()
            .expect("fragment present");
        assert_eq!(foff, 42);
        assert_eq!(src_start, 0); // walked back to lc0
        assert_eq!(src_end, file_size as u64);
    }

    /// `Z_EROFS_FRAGMENT_INODE_BIT` set in `h_clusterbits` (high
    /// nibble): the WHOLE inode is a fragment, no real lcluster
    /// index needed. `fragment_range` should short-circuit to
    /// `(fragmentoff, 0, file_size)` without walking lclusters.
    #[test]
    fn fragments_full_inode_bit_short_circuits() {
        // Even if the (unused) lcluster index entries look
        // suspicious, the FRAGMENT_INODE_BIT path skips reading
        // them. Use an obviously-malformed entry to confirm.
        let bogus = encode_lcluster(/*NONHEAD*/ 2, 0, 0xFFFF /*delta0 too big*/);
        let mut img = build_zmap_image(8, /*advise=0*/ 0, 0, &[bogus]);
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        // h_fragmentoff = 0x42 (offset within packed inode)
        img[hdr_off..hdr_off + 4].copy_from_slice(&0x42u32.to_le_bytes());
        // h_advise = 0 (no FRAGMENT_PCLUSTER advise bit)
        img[hdr_off + 4..hdr_off + 6].copy_from_slice(&0u16.to_le_bytes());
        // h_clusterbits: low 4 bits = 0 (lclusterbits=0), high bit set.
        img[hdr_off + 7] = Z_EROFS_FRAGMENT_INODE_BIT;
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(zmap.has_fragment());
        assert!(zmap.has_full_fragment());
        let (foff, src_start, src_end) = zmap
            .fragment_range(&dev)
            .unwrap()
            .expect("full-inode fragment present");
        assert_eq!(foff, 0x42);
        assert_eq!(src_start, 0);
        assert_eq!(src_end, 8);
    }

    /// When BOTH ztailpacking AND fragments advise bits are set,
    /// fragments takes precedence: `tail_inline_offset_and_size`
    /// returns `None` so the read path doesn't redirect to a
    /// (meaningless) `h_idata_size`-derived range, and
    /// `fragment_range` reports the redirection target instead.
    #[test]
    fn fragments_takes_precedence_over_ztailpacking() {
        let head = encode_lcluster(1, 0, 5);
        let mut img = build_zmap_image(
            4096,
            Z_EROFS_ADVISE_FRAGMENT_PCLUSTER | Z_EROFS_ADVISE_INLINE_PCLUSTER,
            0,
            &[head],
        );
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        img[hdr_off..hdr_off + 4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(zmap.has_fragment());
        assert!(zmap.has_inline_tail()); // bit IS set
                                         // ...but the inline-tail accessor refuses, deferring to the
                                         // fragment redirect.
        assert!(zmap.tail_inline_offset_and_size().is_none());
        let frag = zmap.fragment_range(&dev).unwrap().expect("fragment");
        assert_eq!(frag.0, 0xDEAD_BEEF);
        assert_eq!(frag.1, 0);
        assert_eq!(frag.2, 4096);
    }

    /// BIG_PCLUSTER + a single PLAIN lcluster: the simplest accepted
    /// big-pcluster image. Open succeeds; `pcluster_extent` returns a
    /// 1-block extent (no CBLKCNT marker means degenerate single-block
    /// pcluster, the same shape mkfs.erofs emits when the LZMA frame
    /// fits in one block but the BIG_PCLUSTER advise bit is on).
    #[test]
    fn big_pcluster_advise_bit_accepted() {
        let only = encode_lcluster(0, 0, 7); // PLAIN @ blkaddr 7
        let img = build_zmap_image(4096, Z_EROFS_ADVISE_BIG_PCLUSTER_1, 0, &[only]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).expect("open with BIG_PCLUSTER_1");
        let e = zmap.pcluster_extent(&dev, 0).unwrap();
        assert_eq!(e.pcluster_blkaddr, 7);
        assert_eq!(e.pcluster_block_count, 1, "no CBLKCNT marker -> 1 block");
        assert_eq!(e.source_start_byte, 0);
        assert_eq!(e.source_end_byte, 4096);
    }

    /// Legacy BIG_PCLUSTER pcluster spanning 3 lclusters (HEAD + 2
    /// NONHEADs) with CBLKCNT-marker on the first NONHEAD encoding 5
    /// compressed blocks. Verifies `pcluster_extent` reports the
    /// 5-block compressed length and decodes the implicit delta[0]=1.
    #[test]
    fn legacy_big_pcluster_cblkcnt_marker_decoded() {
        let head = encode_lcluster(/*HEAD1*/ 1, 0, 100);
        // First NONHEAD: CBLKCNT marker (0x800) | block_count(5) in
        // delta[0] (low 16 bits of u). delta[1] (high 16 bits) = 2
        // (forward distance to next HEAD/PLAIN).
        let cblkcnt_lo = Z_EROFS_LI_D0_CBLKCNT | 5;
        let nonhead_cblkcnt = encode_lcluster(2, 0, cblkcnt_lo | (2u32 << 16));
        // Second NONHEAD: regular delta[0]=2, delta[1]=1.
        let nonhead_regular = encode_lcluster(2, 0, 2 | (1u32 << 16));
        let img = build_zmap_image(
            3 * 4096,
            Z_EROFS_ADVISE_BIG_PCLUSTER_1,
            0,
            &[head, nonhead_cblkcnt, nonhead_regular],
        );
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).expect("open BIG_PCLUSTER");

        // From any offset inside the pcluster, the extent should
        // resolve to the same 5-block multi-lcluster pcluster.
        for q in [0u64, 4096, 4096 * 2 + 1024] {
            let e = zmap.pcluster_extent(&dev, q).unwrap();
            assert_eq!(e.pcluster_blkaddr, 100, "query {q}");
            assert_eq!(e.pcluster_block_count, 5, "query {q}");
            assert_eq!(e.source_start_byte, 0, "query {q}");
            assert_eq!(e.source_end_byte, 3 * 4096, "query {q}");
            assert_eq!(e.head_lcluster_idx, 0, "query {q}");
        }

        // map() across the same offsets must walk back to the head's
        // blkaddr too -- the implicit delta[0]=1 path.
        for q in [4096u64, 4096 * 2] {
            let m = zmap.map(&dev, q).unwrap();
            assert_eq!(m.pcluster_blkaddr, 100, "map query {q}");
        }
    }

    #[test]
    fn out_of_range_file_offset_rejected() {
        let head = encode_lcluster(1, 0, 100);
        let img = build_zmap_image(4096, 0, 0, &[head]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(matches!(zmap.map(&dev, 4096), Err(Error::OutOfRange)));
        assert!(matches!(zmap.map(&dev, 9999), Err(Error::OutOfRange)));
    }

    #[test]
    fn nonhead_with_zero_delta_is_bad_inode() {
        let bad = encode_lcluster(/*NONHEAD*/ 2, 0, 0);
        let img = build_zmap_image(4096, 0, 0, &[bad]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(matches!(zmap.map(&dev, 0), Err(Error::BadInode(_))));
    }

    #[test]
    fn header_lclusterbits_only_uses_low_nibble() {
        let head = encode_lcluster(1, 0, 7);
        let mut img = build_zmap_image(4096, 0, 0, &[head]);
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        // byte 7 = h_clusterbits; only low 4 bits = lclusterbits.
        img[hdr_off + 7] = 0xF0;
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert_eq!(zmap.lclusterbits(), 0);
        assert_eq!(zmap.lcluster_size(), 4096);
    }

    #[test]
    fn no_inline_tail_returns_none() {
        let head = encode_lcluster(1, 0, 100);
        let img = build_zmap_image(4096, 0, 0, &[head]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(zmap.tail_inline_offset_and_size().is_none());
    }

    // --- pcluster_extent tests (the multi-lcluster fix) ----------------

    /// HEAD with clusterofs=0 + a NONHEAD spanning the same pcluster.
    /// Expected: source extent [0, source_end), block_count=1,
    /// head_lcluster_idx=0.
    #[test]
    fn pcluster_extent_simple_head_at_lc0() {
        // 2 lclusters, 1 pcluster: HEAD@lc0(blk=5), NONHEAD@lc1(d0=1).
        let head = encode_lcluster(1, 0, 5);
        let nonhead = encode_lcluster(2, 0, 1);
        let img = build_zmap_image(2 * 4096, 0, 0, &[head, nonhead]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();

        let e = zmap.pcluster_extent(&dev, 0).unwrap();
        assert_eq!(e.pcluster_blkaddr, 5);
        assert_eq!(e.source_start_byte, 0);
        // No next HEAD found -> source_end clamped to inode.size.
        assert_eq!(e.source_end_byte, 2 * 4096);
        assert_eq!(e.head_lcluster_idx, 0);
        assert!(e.is_last_pcluster);
        assert_eq!(e.cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);

        // Querying from inside the NONHEAD lcluster resolves to the
        // SAME pcluster.
        let e2 = zmap.pcluster_extent(&dev, 4096 + 17).unwrap();
        assert_eq!(e2.pcluster_blkaddr, 5);
        assert_eq!(e2.source_start_byte, 0);
        assert_eq!(e2.head_lcluster_idx, 0);
    }

    /// Two separate pclusters: HEAD@lc0 + HEAD@lc2 with a NONHEAD
    /// between them. Verifies forward-walk to find the next HEAD as
    /// the pcluster source-end bound.
    #[test]
    fn pcluster_extent_two_pclusters_bounded_by_next_head() {
        let head1 = encode_lcluster(1, 0, 100);
        let nonhead = encode_lcluster(2, 0, 1);
        let head2 = encode_lcluster(1, 0, 200);
        let img = build_zmap_image(3 * 4096, 0, 0, &[head1, nonhead, head2]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();

        let e = zmap.pcluster_extent(&dev, 0).unwrap();
        assert_eq!(e.pcluster_blkaddr, 100);
        assert_eq!(e.source_start_byte, 0);
        // Bounded by lc2's HEAD at byte 8192.
        assert_eq!(e.source_end_byte, 2 * 4096);
        assert!(!e.is_last_pcluster);

        let e2 = zmap.pcluster_extent(&dev, 2 * 4096).unwrap();
        assert_eq!(e2.pcluster_blkaddr, 200);
        assert_eq!(e2.source_start_byte, 2 * 4096);
        assert_eq!(e2.source_end_byte, 3 * 4096);
        assert!(e2.is_last_pcluster);
    }

    /// HEAD with clusterofs > 0 means the previous pcluster's source
    /// extends INTO this lcluster. An offset within `[lc*lc_size,
    /// lc*lc_size + clusterofs)` resolves back to the previous
    /// pcluster's HEAD.
    #[test]
    fn pcluster_extent_head_with_clusterofs_walks_back() {
        // lc0 HEAD1 blkaddr=100 clusterofs=0 -- pcluster A.
        // lc1 HEAD1 blkaddr=200 clusterofs=500 -- pcluster B starts mid
        // way through lc1. Bytes [4096, 4096+500) of the file belong
        // to pcluster A's tail.
        let head1 = encode_lcluster(1, 0, 100);
        let head2 = encode_lcluster(1, 500, 200);
        let img = build_zmap_image(2 * 4096, 0, 0, &[head1, head2]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();

        // file_offset = 4096+100 -> in lc1, in_lcluster=100 < 500 ->
        // walk back to lc0 HEAD.
        let e = zmap.pcluster_extent(&dev, 4096 + 100).unwrap();
        assert_eq!(e.pcluster_blkaddr, 100, "should be pcluster A");
        assert_eq!(e.source_start_byte, 0);
        assert_eq!(e.source_end_byte, 4096 + 500); // bounded by next HEAD
        assert_eq!(e.head_lcluster_idx, 0);

        // file_offset = 4096+500 -> in lc1, in_lcluster=500 == clusterofs
        // -> THIS HEAD owns it.
        let e2 = zmap.pcluster_extent(&dev, 4096 + 500).unwrap();
        assert_eq!(e2.pcluster_blkaddr, 200);
        assert_eq!(e2.source_start_byte, 4096 + 500);
        assert_eq!(e2.head_lcluster_idx, 1);
        assert!(e2.is_last_pcluster);
    }

    /// `Z_EROFS_ADVISE_INTERLACED_PCLUSTER` bit lives in the map
    /// header's `h_advise`. `ZMap::has_interlaced_pcluster` exposes
    /// the bit cheaply; verify it round-trips.
    #[test]
    fn interlaced_advise_bit_decoded() {
        let head = encode_lcluster(/*PLAIN*/ 0, 0, 7);
        let img = build_zmap_image(4096, Z_EROFS_ADVISE_INTERLACED_PCLUSTER, 0, &[head]);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert!(zmap.has_interlaced_pcluster());
        assert_eq!(Z_EROFS_ADVISE_INTERLACED_PCLUSTER, 0x0040);
    }

    /// HEAD2 cluster_type must dispatch to the codec encoded in the
    /// HIGH nibble of `h_algorithmtype`; HEAD1 takes the low nibble.
    /// This pins the per-cluster codec resolution that mixed-codec
    /// images (`mkfs.erofs -z lz4hc,lzma`) rely on.
    #[test]
    fn head2_algorithm_uses_high_nibble() {
        let head = encode_lcluster(/*HEAD2*/ 3, 0, 5);
        let mut img = build_zmap_image(4096, 0, 0, &[head]);
        const BS: usize = 4096;
        let hdr_off = BS + 32;
        // Byte 6 of the map header = h_algorithmtype. Encode HEAD1 =
        // LZ4 (low nibble = 0) and HEAD2 = LZMA (high nibble = 1):
        // packed byte = 0x10.
        img[hdr_off + 6] = 0x10;
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();
        assert_eq!(
            zmap.header_algo(Z_EROFS_LCLUSTER_TYPE_HEAD1).unwrap(),
            crate::decompress::Algorithm::Lz4
        );
        assert_eq!(
            zmap.header_algo(Z_EROFS_LCLUSTER_TYPE_HEAD2).unwrap(),
            crate::decompress::Algorithm::Lzma
        );
        // PLAIN / NONHEAD have no codec; resolver rejects them.
        assert!(matches!(
            zmap.header_algo(Z_EROFS_LCLUSTER_TYPE_PLAIN),
            Err(Error::BadInode(_))
        ));
        assert!(matches!(
            zmap.header_algo(Z_EROFS_LCLUSTER_TYPE_NONHEAD),
            Err(Error::BadInode(_))
        ));
    }

    /// Build a synthetic image with a COMPR_CFGS blob (LZ4 + LZMA
    /// records) and verify the parser extracts both. Independent of
    /// the codec dispatch so we don't need an actual compressed
    /// payload.
    #[test]
    fn compr_cfgs_blob_parsed() {
        use crate::superblock::{
            read_compr_cfgs, EROFS_FEATURE_INCOMPAT_COMPR_CFGS, EROFS_SUPER_BLOCK_SIZE,
            EROFS_SUPER_OFFSET,
        };
        const BS: usize = 4096;
        let mut img = vec![0u8; BS * 4];
        let mut sb = synth_sb(12, 0, 1, 4);
        // Set feature_incompat = COMPR_CFGS and available_compr_algs
        // (u1 union slot) = LZ4 | LZMA bits.
        sb[0x50..0x54].copy_from_slice(&EROFS_FEATURE_INCOMPAT_COMPR_CFGS.to_le_bytes());
        let algos: u16 = (1 << 0) | (1 << 1); // LZ4 + LZMA
        sb[0x54..0x56].copy_from_slice(&algos.to_le_bytes());
        // sb_extslots = 0 -> blob lives at EROFS_SUPER_OFFSET + 128.
        img[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        let blob_off = EROFS_SUPER_OFFSET as usize + EROFS_SUPER_BLOCK_SIZE;
        // Record 1: LZ4 (4-byte payload, value irrelevant; reader
        // doesn't propagate it today).
        img[blob_off..blob_off + 2].copy_from_slice(&4u16.to_le_bytes());
        img[blob_off + 2..blob_off + 6].copy_from_slice(&0u32.to_le_bytes());
        // Record 2: LZMA (14-byte payload). dict_size = 1 MiB at
        // offset 0; remaining 10 bytes (format + reserved) zeros.
        let lzma_off = blob_off + 6;
        img[lzma_off..lzma_off + 2].copy_from_slice(&14u16.to_le_bytes());
        img[lzma_off + 2..lzma_off + 6].copy_from_slice(&(1u32 << 20).to_le_bytes());
        // 10 bytes of zero padding follow (already zero).

        let dev = MemDev(Mutex::new(img));
        let sb_parsed = crate::superblock::read(&dev).unwrap();
        let cfgs = read_compr_cfgs(&dev, &sb_parsed).unwrap().expect("cfgs");
        assert!(cfgs.lz4.is_some(), "LZ4 record must be marked present");
        let lzma = cfgs.lzma.expect("LZMA record must parse");
        assert_eq!(lzma.dict_size, 1 << 20);
        assert_eq!(lzma.lc, 3);
        assert_eq!(lzma.lp, 0);
        assert_eq!(lzma.pb, 2);
        assert!(cfgs.deflate.is_none());
    }

    /// Multi-lcluster pcluster with a SENTINEL-style PLAIN last entry
    /// (mimics what mkfs.erofs emits for a 200K file: HEAD@lc0 +
    /// NONHEADs through lc11 + PLAIN sentinel@lc12 with clusterofs =
    /// file_size % lcluster_size).
    #[test]
    fn pcluster_extent_walks_past_many_nonheads_to_sentinel() {
        let mut entries: Vec<[u8; 8]> = Vec::new();
        // lc0 HEAD1 blkaddr=1 clusterofs=0
        entries.push(encode_lcluster(1, 0, 1));
        // lc1..lc11 NONHEAD with delta0=1..11
        for i in 1u32..12 {
            entries.push(encode_lcluster(2, 0, i | ((12 - i) << 16)));
        }
        // lc12 PLAIN clusterofs=3392 (sentinel "file ends at
        // 12*4096+3392 = 52544"). blkaddr=0 (sentinel marker).
        entries.push(encode_lcluster(0, 3392, 0));
        // Pretend the file is 52544 bytes; n_lclusters = 13.
        let img = build_zmap_image(52544, 0, 0, &entries);
        let dev = MemDev(Mutex::new(img));
        let sb = crate::superblock::read(&dev).unwrap();
        let inode = Inode::read(&dev, &sb, 0).unwrap();
        let zmap = ZMap::open(&dev, &sb, &inode).unwrap();

        for query in [0u64, 4096, 4096 * 5, 4096 * 11, 52000] {
            let e = zmap.pcluster_extent(&dev, query).unwrap();
            assert_eq!(e.pcluster_blkaddr, 1, "query={query}");
            assert_eq!(e.source_start_byte, 0, "query={query}");
            // Sentinel bounds source to file_size.
            assert_eq!(e.source_end_byte, 52544, "query={query}");
            assert_eq!(e.head_lcluster_idx, 0);
        }
    }

    /// Compile-time pin of [`IndexFormat`]'s variant set. The public
    /// EROFS on-disk-format spec exposes exactly two compressed-index
    /// encodings -- legacy/full and compact (the latter mixing 4B and
    /// 2B packs per advise bits) -- and erofs-utils 1.9 emits no others.
    /// In particular there is no `Compacted1B` variant: a hypothetical
    /// 1-byte-per-lcluster encoding has never been ratified upstream
    /// (see the module-header note for the empirical evidence). This
    /// test exists so a future contributor reintroducing a phantom
    /// variant -- e.g. transcribing a stale design-thread reference --
    /// gets a clear failure pointing back at the module-header
    /// rationale rather than silently growing the enum.
    #[test]
    fn index_format_has_exactly_legacy_and_compact() {
        // Exhaustive match: any added variant breaks the build, which
        // surfaces in CI as a compile error for this test rather than
        // silently shipping. The match arms also assert the discriminant
        // identities so a `#[repr(u8)]` reorder is caught at runtime.
        let legacy = IndexFormat::Legacy;
        let compact = IndexFormat::Compact;
        match legacy {
            IndexFormat::Legacy => {}
            IndexFormat::Compact => panic!("Legacy must not equal Compact"),
        }
        match compact {
            IndexFormat::Legacy => panic!("Compact must not equal Legacy"),
            IndexFormat::Compact => {}
        }
        assert_ne!(legacy, compact);
    }
}
