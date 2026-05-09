//! EROFS superblock parsing.
//!
//! The superblock lives at byte offset 1024 (`EROFS_SUPER_OFFSET`) and is
//! 128 bytes long. Field layout matches `struct erofs_super_block` in
//! `linux/fs/erofs/erofs_fs.h` -- names are kept verbatim so cross-
//! referencing the kernel header stays mechanical.
//!
//! All on-disk fields are little-endian. We extract a small typed struct
//! rather than holding the raw bytes; consumers that want raw access
//! can reread via `BlockRead::read_at(EROFS_SUPER_OFFSET, ...)`.

use crate::error::{Error, Result};
use fs_core::BlockRead;

pub const EROFS_SUPER_OFFSET: u64 = 1024;
pub const EROFS_SUPER_BLOCK_SIZE: usize = 128;
pub const EROFS_SUPER_MAGIC_V1: u32 = 0xE0F5_E1E2;

/// `feature_compat` bit 0. When set, [`Superblock::checksum`] holds the
/// CRC32C of the 128-byte superblock with the checksum field itself
/// treated as zeros during computation.
///
/// Spec: `linux/fs/erofs/erofs_fs.h::EROFS_FEATURE_COMPAT_SB_CHKSUM`.
pub const EROFS_FEATURE_COMPAT_SB_CHKSUM: u32 = 0x0000_0001;

/// `feature_incompat` bit advertising cross-file fragment-pcluster
/// (packed-tail) support. When set, `Superblock::packed_nid` carries
/// the NID of the special "packed inode" that holds the collated tail
/// bytes for every file with a `Z_EROFS_ADVISE_FRAGMENT_PCLUSTER`
/// header bit. Spec: `linux/fs/erofs/erofs_fs.h::
/// EROFS_FEATURE_INCOMPAT_FRAGMENTS`.
pub const EROFS_FEATURE_INCOMPAT_FRAGMENTS: u32 = 0x0000_0010;

/// `feature_incompat` bit advertising the per-algorithm "compression
/// configurations" blob that lives immediately after the 128-byte
/// superblock + extension slots. When set, the post-SB blob carries a
/// sequence of `__le16 size; __le16 type; size bytes payload` records
/// (terminated by a record with `size == 0`) that supply per-codec
/// parameters (e.g. LZMA `dict_size`, `lc`, `lp`, `pb`) the reader
/// must plumb into the codec.
///
/// The bit value is `0x0000_0002` in the public `erofs_fs.h` header
/// (alias of `EROFS_FEATURE_INCOMPAT_BIG_PCLUSTER`; the kernel
/// renamed the older alias in place when the cfgs blob was added).
/// Empirically confirmed against `mkfs.erofs -z lzma` output on
/// erofs-utils 1.9 (image's `feature_incompat == 0x3`, i.e.
/// `ZERO_PADDING | COMPR_CFGS`).
///
/// Spec source: public EROFS kernel header constants in
/// `linux/fs/erofs/erofs_fs.h` and the on-disk-format chapter of the
/// public EROFS documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html>).
/// Independent implementation; license clean.
pub const EROFS_FEATURE_INCOMPAT_COMPR_CFGS: u32 = 0x0000_0002;

/// Per-algorithm configuration parsed from the COMPR_CFGS blob. One
/// entry per codec type id observed in the blob. Only LZMA carries
/// reader-relevant parameters today (LZ4 / DEFLATE blobs exist but
/// are empty / informational).
///
/// Spec: `Z_EROFS_COMPRESSION_*` type ids in the public format header
/// `erofs_fs.h` and the on-disk-format chapter of the public EROFS
/// documentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComprCfgs {
    /// `Z_EROFS_COMPRESSION_LZ4 = 0`. Empty / no parameters today.
    pub lz4: Option<()>,
    /// `Z_EROFS_COMPRESSION_LZMA = 1`. Carries `dict_size` / `lc` /
    /// `lp` / `pb`.
    pub lzma: Option<LzmaCfg>,
    /// `Z_EROFS_COMPRESSION_DEFLATE = 2`. Empty today (window-bits is
    /// fixed at the EROFS / kernel default).
    pub deflate: Option<()>,
}

/// Decoded LZMA configuration record from the COMPR_CFGS blob.
///
/// Layout of `z_erofs_lzma_cfgs` (per the public format header
/// `erofs_fs.h`): `__le32 dict_size; __le16 format; u8 reserved[8];`.
/// We only need `dict_size`; the other fields are reserved for future
/// use. Properties byte (lc, lp, pb) is carried as a packed `u8` with
/// the LZMA1 layout `byte = (pb * 5 + lp) * 9 + lc`. Modern mkfs.erofs
/// always emits the LZMA1 defaults `(lc=3, lp=0, pb=2)` so we
/// short-circuit to those when the blob doesn't override; the spec
/// reserves room for non-default props but we have not seen them in
/// practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzmaCfg {
    pub dict_size: u32,
    pub lc: u8,
    pub lp: u8,
    pub pb: u8,
}

impl Default for LzmaCfg {
    fn default() -> Self {
        // Standard LZMA1 defaults: properties = 0x5d.
        LzmaCfg {
            dict_size: 1u32 << 24,
            lc: 3,
            lp: 0,
            pb: 2,
        }
    }
}

/// Maximum reasonable block size shift. EROFS supports 9..=16 in theory
/// (512 B .. 64 KiB blocks); the common case is 12 (4 KiB). We reject
/// values outside this range to catch corrupt headers cheaply.
const MIN_BLKSZBITS: u8 = 9;
const MAX_BLKSZBITS: u8 = 16;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub magic: u32,
    pub checksum: u32,
    pub feature_compat: u32,
    /// log2(block size). Block size in bytes is `1 << blkszbits`.
    pub blkszbits: u8,
    /// Extension slots beyond the 128-byte base. Total SB size is
    /// `128 + sb_extslots * 16`. Phase 0 ignores extension slots.
    pub sb_extslots: u8,
    /// NID of the root directory inode.
    pub root_nid: u16,
    pub inos: u64,
    pub build_time: u64,
    pub build_time_nsec: u32,
    /// Total blocks in the filesystem.
    pub blocks: u32,
    /// Block address of the metadata area (where inodes live).
    pub meta_blkaddr: u32,
    /// Block address of the xattr area.
    pub xattr_blkaddr: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub feature_incompat: u32,
    /// Either `available_compr_algs` (compression bits) or
    /// `lz4_max_distance` -- a union in the C header. Phase 0 keeps the
    /// raw u16 and lets higher layers interpret as needed.
    pub u1: u16,
    pub extra_devices: u16,
    pub devt_slotoff: u16,
    pub dirblkbits: u8,
    pub xattr_prefix_count: u8,
    pub xattr_prefix_start: u32,
    pub packed_nid: u64,
}

impl Superblock {
    /// Parse + validate a superblock from a 128-byte buffer. Caller is
    /// responsible for reading the buffer at `EROFS_SUPER_OFFSET`; this
    /// function is intentionally byte-slice-only so it's trivially
    /// testable with a synthetic image.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < EROFS_SUPER_BLOCK_SIZE {
            return Err(Error::BadSuperblock("buffer shorter than 128 bytes"));
        }

        let magic = u32::from_le_bytes(bytes[0x00..0x04].try_into().unwrap());
        if magic != EROFS_SUPER_MAGIC_V1 {
            return Err(Error::NotErofs);
        }

        let blkszbits = bytes[0x0C];
        if !(MIN_BLKSZBITS..=MAX_BLKSZBITS).contains(&blkszbits) {
            return Err(Error::BadSuperblock("blkszbits out of range"));
        }

        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes[0x30..0x40]);
        let mut volume_name = [0u8; 16];
        volume_name.copy_from_slice(&bytes[0x40..0x50]);

        Ok(Superblock {
            magic,
            checksum: u32::from_le_bytes(bytes[0x04..0x08].try_into().unwrap()),
            feature_compat: u32::from_le_bytes(bytes[0x08..0x0C].try_into().unwrap()),
            blkszbits,
            sb_extslots: bytes[0x0D],
            root_nid: u16::from_le_bytes(bytes[0x0E..0x10].try_into().unwrap()),
            inos: u64::from_le_bytes(bytes[0x10..0x18].try_into().unwrap()),
            build_time: u64::from_le_bytes(bytes[0x18..0x20].try_into().unwrap()),
            build_time_nsec: u32::from_le_bytes(bytes[0x20..0x24].try_into().unwrap()),
            blocks: u32::from_le_bytes(bytes[0x24..0x28].try_into().unwrap()),
            meta_blkaddr: u32::from_le_bytes(bytes[0x28..0x2C].try_into().unwrap()),
            xattr_blkaddr: u32::from_le_bytes(bytes[0x2C..0x30].try_into().unwrap()),
            uuid,
            volume_name,
            feature_incompat: u32::from_le_bytes(bytes[0x50..0x54].try_into().unwrap()),
            u1: u16::from_le_bytes(bytes[0x54..0x56].try_into().unwrap()),
            extra_devices: u16::from_le_bytes(bytes[0x56..0x58].try_into().unwrap()),
            devt_slotoff: u16::from_le_bytes(bytes[0x58..0x5A].try_into().unwrap()),
            dirblkbits: bytes[0x5A],
            xattr_prefix_count: bytes[0x5B],
            xattr_prefix_start: u32::from_le_bytes(bytes[0x5C..0x60].try_into().unwrap()),
            packed_nid: u64::from_le_bytes(bytes[0x60..0x68].try_into().unwrap()),
        })
    }

    pub fn block_size(&self) -> u64 {
        1u64 << self.blkszbits
    }

    /// Volume name as a UTF-8 string. EROFS pads with zeros; we trim.
    pub fn volume_name_str(&self) -> &str {
        let end = self
            .volume_name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.volume_name.len());
        std::str::from_utf8(&self.volume_name[..end]).unwrap_or("")
    }

    /// Best-effort CRC32C verification of the on-disk superblock. Returns
    /// `true` if `EROFS_FEATURE_COMPAT_SB_CHKSUM` is clear (no checksum
    /// to verify) OR the recomputed CRC32C matches `self.checksum`.
    ///
    /// The CRC is computed over the 128-byte SB with the 4-byte checksum
    /// field at offset 0x04..0x08 zeroed for the calculation. Callers
    /// pass `raw_sb` -- the exact 128 bytes read from
    /// `EROFS_SUPER_OFFSET`. We deliberately don't gate `parse` on this
    /// (older mkfs.erofs images don't set the bit) -- it's an opt-in
    /// integrity check.
    ///
    /// Algorithm: CRC32C (Castagnoli, RFC 3720) over the 128-byte
    /// superblock with the 4-byte checksum field at offset 0x04..0x08
    /// treated as zero during the calculation. Conveyed by the public
    /// EROFS on-disk format documentation
    /// (<https://erofs.docs.kernel.org/en/latest/design.html>).
    /// Independent implementation.
    pub fn verify_checksum(&self, raw_sb: &[u8]) -> bool {
        if self.feature_compat & EROFS_FEATURE_COMPAT_SB_CHKSUM == 0 {
            return true;
        }
        if raw_sb.len() < EROFS_SUPER_BLOCK_SIZE {
            return false;
        }
        let mut tmp = [0u8; EROFS_SUPER_BLOCK_SIZE];
        tmp.copy_from_slice(&raw_sb[..EROFS_SUPER_BLOCK_SIZE]);
        // Zero the checksum field for recomputation.
        tmp[0x04..0x08].fill(0);
        crc32c::crc32c(&tmp) == self.checksum
    }
}

/// Read the superblock from a block device.
pub fn read<R: BlockRead + ?Sized>(dev: &R) -> Result<Superblock> {
    let mut buf = [0u8; EROFS_SUPER_BLOCK_SIZE];
    dev.read_at(EROFS_SUPER_OFFSET, &mut buf)?;
    Superblock::parse(&buf)
}

/// Size of one on-disk `erofs_deviceslot` entry. The table is an array
/// of these slots, indexed by `device_id - 1` (device_id 0 refers to
/// the primary / hosting device, which has no slot of its own).
pub const EROFS_DEVT_SLOT_SIZE: u64 = 128;

/// Decoded entry from the on-disk device table.
///
/// Layout (little-endian, 128 bytes):
///   `u8 tag[64]; __le32 blocks; __le32 mapped_blkaddr; u8 reserved[56];`
///
/// `tag` is a consumer-defined identifier (URI, GUID, partition name,
/// ...). `blocks` is the size of the device in EROFS blocks. The reader
/// only needs `tag` to let callers cross-reference the slot with a real
/// backing handle they pass in via [`super::Filesystem::open_with_devices`];
/// `blocks` and `mapped_blkaddr` are surfaced for completeness so
/// integrators can sanity-check geometry.
///
/// Spec: public EROFS on-disk-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html>).
/// Independent implementation; license clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSlot {
    /// Consumer-defined identifier. Null-padded; trim before display.
    pub tag: [u8; 64],
    /// Device size in EROFS blocks.
    pub blocks: u32,
    /// Start block address of this device when it is itself an EROFS
    /// slice within a larger host filesystem. `0` for standalone devices.
    pub mapped_blkaddr: u32,
}

impl DeviceSlot {
    /// `tag` as a human-readable string, trimming trailing NULs. Falls
    /// back to an empty string if the tag bytes aren't valid UTF-8.
    pub fn tag_str(&self) -> &str {
        let end = self
            .tag
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.tag.len());
        std::str::from_utf8(&self.tag[..end]).unwrap_or("")
    }
}

/// Byte offset of the on-disk device table. Per the public EROFS
/// on-disk-format documentation the table starts at
/// `devt_slotoff * EROFS_DEVT_SLOT_SIZE` bytes from the device origin
/// (0). `devt_slotoff` is a count of 128-byte slots; the value is
/// chosen by mkfs so that the table sits beyond the superblock + any
/// extension slots + the COMPR_CFGS blob. The reader trusts the SB
/// value rather than re-deriving the layout.
pub fn device_table_offset(sb: &Superblock) -> u64 {
    (sb.devt_slotoff as u64) * EROFS_DEVT_SLOT_SIZE
}

/// Read the on-disk device table. Returns `sb.extra_devices` slots, in
/// order; index `k` corresponds to `device_id == k + 1` (since
/// `device_id == 0` is the primary / hosting device, which has no
/// slot).
///
/// Returns an empty Vec when `sb.extra_devices == 0`. A
/// short / unreadable table surfaces the underlying I/O error from
/// `BlockRead`.
///
/// Spec: public EROFS on-disk-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html>).
/// Independent implementation; license clean.
pub fn read_device_table<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
) -> Result<Vec<DeviceSlot>> {
    let n = sb.extra_devices as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    let base = device_table_offset(sb);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = base + (i as u64) * EROFS_DEVT_SLOT_SIZE;
        let mut buf = [0u8; EROFS_DEVT_SLOT_SIZE as usize];
        dev.read_at(off, &mut buf)?;
        let mut tag = [0u8; 64];
        tag.copy_from_slice(&buf[0..64]);
        let blocks = u32::from_le_bytes(buf[64..68].try_into().unwrap());
        let mapped_blkaddr = u32::from_le_bytes(buf[68..72].try_into().unwrap());
        out.push(DeviceSlot {
            tag,
            blocks,
            mapped_blkaddr,
        });
    }
    Ok(out)
}

/// Byte offset of the post-superblock COMPR_CFGS blob: immediately
/// after the 128-byte SB plus any extension slots.
pub fn compr_cfgs_offset(sb: &Superblock) -> u64 {
    EROFS_SUPER_OFFSET + EROFS_SUPER_BLOCK_SIZE as u64 + (sb.sb_extslots as u64) * 16
}

/// Hard cap on the size of the COMPR_CFGS blob we'll walk. Each
/// record is `__le16 size + size bytes`; we cap the total walk at a
/// few KiB to bound I/O. Eight 64KiB records (~512 KiB) is far more
/// than any real image needs (LZMA cfgs is 14 bytes, DEFLATE cfgs is
/// 6 bytes).
const COMPR_CFGS_MAX_BYTES: u64 = 8 * (2 + 65535);

/// Algorithm bit positions in `Superblock::u1` when the field is
/// interpreted as `available_compr_algs` (i.e. on images with
/// `EROFS_FEATURE_INCOMPAT_COMPR_CFGS` set). Empirically confirmed
/// against `mkfs.erofs -z {lzma,deflate,lz4}` on erofs-utils 1.9
/// (LZMA image: u1 == 0x02 = bit 1; DEFLATE image: u1 == 0x04 = bit
/// 2; LZ4 image: COMPR_CFGS feature bit is clear so `u1` is the
/// `lz4_max_distance` union arm instead).
const Z_EROFS_COMPRESSION_LZ4_BIT: u16 = 1 << 0;
const Z_EROFS_COMPRESSION_LZMA_BIT: u16 = 1 << 1;
const Z_EROFS_COMPRESSION_DEFLATE_BIT: u16 = 1 << 2;
const Z_EROFS_COMPRESSION_ZSTD_BIT: u16 = 1 << 3;

/// Parse the post-superblock COMPR_CFGS blob if the image advertises
/// it. Returns `Ok(None)` when the feature bit is clear (the common
/// case for older / single-codec / LZ4-only images).
///
/// Format: a sequence of `__le16 size; u8 payload[size];` records,
/// one record per codec, in the canonical codec order (LZ4, LZMA,
/// DEFLATE, ZSTD). Records are present only for codecs whose bit is
/// set in the SB's `available_compr_algs` field (which lives in the
/// `u1` union slot when the COMPR_CFGS feature bit is set). The
/// terminator is the codec list itself ending — there is no
/// `size == 0` sentinel.
///
/// Per-codec payload layouts (taken from the public format header
/// `erofs_fs.h` plus empirical validation against erofs-utils 1.9):
///
/// - LZ4 (`size = 4`): `__le16 max_distance; __le16 max_pcluster_blks;`
///   The reader's LZ4 codec doesn't currently use either parameter
///   (we lean on `lz4_flex` and accept whatever max_distance the
///   writer chose), so this record is parsed but its values aren't
///   propagated.
/// - LZMA (`size = 14`): `__le32 dict_size; __le16 format; u8
///   reserved[8];`. Only `dict_size` flows into the codec; `format`
///   reserves bits for future use. lc / lp / pb are NOT carried in
///   the blob today (mkfs.erofs hard-codes the LZMA1 defaults `(3, 0,
///   2)`); we synthesise those from [`LzmaCfg::default`].
/// - DEFLATE (`size = 6`): `u8 windowbits; u8 reserved[5];`.
///   `windowbits` is informational; the reader's DEFLATE codec
///   accepts any compliant stream.
/// - ZSTD: not implemented. Returns `Error::UnsupportedLayout(3)`.
///
/// Spec: blob layout described in the public EROFS on-disk-format
/// documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html>); per-codec
/// struct field names taken from the public `erofs_fs.h` constants.
/// Independent implementation; license clean.
pub fn read_compr_cfgs<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
) -> Result<Option<ComprCfgs>> {
    if sb.feature_incompat & EROFS_FEATURE_INCOMPAT_COMPR_CFGS == 0 {
        return Ok(None);
    }
    let mut cfgs = ComprCfgs::default();
    let algos = sb.u1; // available_compr_algs bitmap when COMPR_CFGS is on
    let mut cursor = compr_cfgs_offset(sb);
    let stop = cursor + COMPR_CFGS_MAX_BYTES;

    // Walk codecs in canonical order (LZ4, LZMA, DEFLATE, ZSTD); each
    // codec whose bit is set in `available_compr_algs` contributes one
    // record. `read_one` consumes the leading `__le16 size`, then
    // reads `size` payload bytes.
    let read_one = |cursor: &mut u64| -> Result<Vec<u8>> {
        if *cursor + 2 > stop {
            return Err(Error::BadInode("COMPR_CFGS walk exceeded sanity bound"));
        }
        let mut sz_bytes = [0u8; 2];
        dev.read_at(*cursor, &mut sz_bytes)?;
        let size = u16::from_le_bytes(sz_bytes) as usize;
        *cursor += 2;
        let mut payload = vec![0u8; size];
        if size > 0 {
            dev.read_at(*cursor, &mut payload)?;
            *cursor += size as u64;
        }
        Ok(payload)
    };

    if (algos & Z_EROFS_COMPRESSION_LZ4_BIT) != 0 {
        // Consume the LZ4 record; we don't propagate its fields today.
        let _ = read_one(&mut cursor)?;
        cfgs.lz4 = Some(());
    }
    if (algos & Z_EROFS_COMPRESSION_LZMA_BIT) != 0 {
        let payload = read_one(&mut cursor)?;
        if payload.len() < 4 {
            return Err(Error::BadInode("LZMA cfg payload < 4 bytes"));
        }
        let dict_size = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        // mkfs.erofs occasionally emits dict_size = 0 to mean "use the
        // codec default"; the codec then synthesises a header with the
        // LZMA1 default dict (1 << 24). We mirror that by falling back
        // to the default when the on-disk value is zero.
        let dict_size = if dict_size == 0 {
            LzmaCfg::default().dict_size
        } else {
            dict_size
        };
        cfgs.lzma = Some(LzmaCfg {
            dict_size,
            ..LzmaCfg::default()
        });
    }
    if (algos & Z_EROFS_COMPRESSION_DEFLATE_BIT) != 0 {
        let _ = read_one(&mut cursor)?;
        cfgs.deflate = Some(());
    }
    if (algos & Z_EROFS_COMPRESSION_ZSTD_BIT) != 0 {
        return Err(Error::UnsupportedLayout(3));
    }
    Ok(Some(cfgs))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal valid superblock buffer for tests.
    pub(crate) fn synth_sb(
        blkszbits: u8,
        root_nid: u16,
        meta_blkaddr: u32,
        blocks: u32,
    ) -> [u8; EROFS_SUPER_BLOCK_SIZE] {
        let mut b = [0u8; EROFS_SUPER_BLOCK_SIZE];
        b[0x00..0x04].copy_from_slice(&EROFS_SUPER_MAGIC_V1.to_le_bytes());
        b[0x0C] = blkszbits;
        b[0x0E..0x10].copy_from_slice(&root_nid.to_le_bytes());
        b[0x24..0x28].copy_from_slice(&blocks.to_le_bytes());
        b[0x28..0x2C].copy_from_slice(&meta_blkaddr.to_le_bytes());
        b[0x40..0x44].copy_from_slice(b"test");
        b
    }

    #[test]
    fn parse_minimal_superblock() {
        let buf = synth_sb(12, 36, 4, 16);
        let sb = Superblock::parse(&buf).unwrap();
        assert_eq!(sb.magic, EROFS_SUPER_MAGIC_V1);
        assert_eq!(sb.blkszbits, 12);
        assert_eq!(sb.block_size(), 4096);
        assert_eq!(sb.root_nid, 36);
        assert_eq!(sb.meta_blkaddr, 4);
        assert_eq!(sb.blocks, 16);
        assert_eq!(sb.volume_name_str(), "test");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = synth_sb(12, 36, 4, 16);
        buf[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(matches!(Superblock::parse(&buf), Err(Error::NotErofs)));
    }

    #[test]
    fn rejects_bad_blkszbits() {
        let buf = synth_sb(2, 36, 4, 16);
        assert!(matches!(
            Superblock::parse(&buf),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_buffer() {
        let buf = [0u8; 64];
        assert!(matches!(
            Superblock::parse(&buf),
            Err(Error::BadSuperblock(_))
        ));
    }

    /// In-memory device for synthetic device-table tests.
    struct MemDev(Vec<u8>);
    impl BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(fs_core::Error::ShortRead {
                    offset,
                    want: buf.len(),
                    got: self.0.len().saturating_sub(start),
                });
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// Build a synthetic SB buffer with `extra_devices` and `devt_slotoff`
    /// populated.
    fn synth_sb_with_devices(extra: u16, devt_slotoff: u16) -> [u8; EROFS_SUPER_BLOCK_SIZE] {
        let mut b = synth_sb(12, 0, 1, 16);
        b[0x56..0x58].copy_from_slice(&extra.to_le_bytes());
        b[0x58..0x5A].copy_from_slice(&devt_slotoff.to_le_bytes());
        b
    }

    #[test]
    fn device_table_empty_when_extra_devices_zero() {
        let buf = synth_sb_with_devices(0, 0);
        let sb = Superblock::parse(&buf).unwrap();
        let dev = MemDev(buf.to_vec());
        let slots = read_device_table(&dev, &sb).unwrap();
        assert!(slots.is_empty());
    }

    #[test]
    fn device_table_parses() {
        // Build an image large enough to hold the SB + a 2-slot device
        // table at slot offset 16 (= byte offset 16 * 128 = 2048, just
        // past the SB).
        const SLOT_OFF: u16 = 16; // byte 2048
        let mut img = vec![0u8; 4096];
        let sb_buf = synth_sb_with_devices(2, SLOT_OFF);
        img[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb_buf.len()]
            .copy_from_slice(&sb_buf);
        // Slot 0 (device_id = 1):
        let s0 = (SLOT_OFF as usize) * 128;
        img[s0..s0 + 6].copy_from_slice(b"first\0");
        img[s0 + 64..s0 + 68].copy_from_slice(&100u32.to_le_bytes()); // blocks
        img[s0 + 68..s0 + 72].copy_from_slice(&0u32.to_le_bytes()); // mapped_blkaddr
                                                                    // Slot 1 (device_id = 2):
        let s1 = s0 + 128;
        img[s1..s1 + 6].copy_from_slice(b"secnd\0");
        img[s1 + 64..s1 + 68].copy_from_slice(&200u32.to_le_bytes());
        img[s1 + 68..s1 + 72].copy_from_slice(&5u32.to_le_bytes());

        let sb = Superblock::parse(&sb_buf).unwrap();
        let dev = MemDev(img);
        let slots = read_device_table(&dev, &sb).unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].tag_str(), "first");
        assert_eq!(slots[0].blocks, 100);
        assert_eq!(slots[0].mapped_blkaddr, 0);
        assert_eq!(slots[1].tag_str(), "secnd");
        assert_eq!(slots[1].blocks, 200);
        assert_eq!(slots[1].mapped_blkaddr, 5);
    }

    #[test]
    fn device_table_short_read_propagates() {
        // SB says 2 extra devices, but the image is too small to hold
        // the table. read_device_table must surface a ShortRead I/O
        // error rather than panicking.
        const SLOT_OFF: u16 = 16; // byte 2048
        let mut img = vec![0u8; 2048]; // not enough room for any slot
        let sb_buf = synth_sb_with_devices(2, SLOT_OFF);
        img[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb_buf.len()]
            .copy_from_slice(&sb_buf);
        let sb = Superblock::parse(&sb_buf).unwrap();
        let dev = MemDev(img);
        let err = read_device_table(&dev, &sb).unwrap_err();
        assert!(matches!(err, Error::Block(_)));
    }

    #[test]
    fn device_slot_tag_str_handles_unterminated() {
        let slot = DeviceSlot {
            tag: [b'x'; 64],
            blocks: 1,
            mapped_blkaddr: 0,
        };
        // No NUL terminator in 64 bytes: tag_str reads the whole thing.
        assert_eq!(slot.tag_str().len(), 64);
    }
}
