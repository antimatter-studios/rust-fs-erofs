//! EROFS inode `i_format` decoding.
//!
//! `i_format` is a u16 packing three fields:
//!
//! - bit 0:    inode version. 0 = compact (32 bytes), 1 = extended (64 bytes).
//! - bits 1..=3: data layout (the `EROFS_INODE_*` constants).
//! - bits 4..=15: per-layout flags. For compressed layouts, this carries
//!   the compression algorithm; for chunked, the chunk-bit count. Phase 0
//!   surfaces it as a raw u16 since we don't decode compressed/chunked.
//!
//! Field positions defined in `linux/fs/erofs/erofs_fs.h`
//! (`EROFS_I_VERSION_BIT`, `EROFS_I_DATALAYOUT_BIT`, etc.).

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeVersion {
    Compact = 0,
    Extended = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataLayout {
    /// Contiguous raw blocks starting at `i_u.raw_blkaddr`.
    FlatPlain = 0,
    /// Legacy compressed layout. Not supported in Phase 0.
    CompressionLegacy = 1,
    /// Whole blocks plain, last (tail) block inlined immediately after
    /// the inode in the metadata area.
    FlatInline = 2,
    /// Modern compressed layout. Not supported in Phase 0.
    Compression = 3,
    /// Chunk-based; for sparse / huge files. Not supported in Phase 0.
    ChunkBased = 4,
}

impl DataLayout {
    fn from_bits(b: u8) -> Result<Self> {
        match b {
            0 => Ok(DataLayout::FlatPlain),
            1 => Ok(DataLayout::CompressionLegacy),
            2 => Ok(DataLayout::FlatInline),
            3 => Ok(DataLayout::Compression),
            4 => Ok(DataLayout::ChunkBased),
            n => Err(Error::BadInode(if n > 7 {
                "data-layout bits out of range"
            } else {
                "unknown data layout"
            })),
        }
    }

    pub fn is_supported_phase0(self) -> bool {
        matches!(self, DataLayout::FlatPlain | DataLayout::FlatInline)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InodeFormat {
    pub version: InodeVersion,
    pub layout: DataLayout,
    /// Per-layout flag bits (4..=15). Phase 0 only reads this for
    /// diagnostics.
    pub flags: u16,
}

impl InodeFormat {
    pub fn parse(raw: u16) -> Result<Self> {
        let version = if raw & 1 == 0 {
            InodeVersion::Compact
        } else {
            InodeVersion::Extended
        };
        let layout_bits = ((raw >> 1) & 0b111) as u8;
        let layout = DataLayout::from_bits(layout_bits)?;
        let flags = raw >> 4;
        Ok(InodeFormat {
            version,
            layout,
            flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_flat_plain() {
        let f = InodeFormat::parse(0b0000_0000).unwrap();
        assert_eq!(f.version, InodeVersion::Compact);
        assert_eq!(f.layout, DataLayout::FlatPlain);
        assert_eq!(f.flags, 0);
    }

    #[test]
    fn extended_flat_inline() {
        // version=1, layout=2 -> raw bits 0b0101 = 0x05
        let f = InodeFormat::parse(0b0000_0101).unwrap();
        assert_eq!(f.version, InodeVersion::Extended);
        assert_eq!(f.layout, DataLayout::FlatInline);
    }

    #[test]
    fn compression_layout_decodes_but_unsupported() {
        // version=0, layout=3
        let f = InodeFormat::parse(0b0000_0110).unwrap();
        assert_eq!(f.layout, DataLayout::Compression);
        assert!(!f.layout.is_supported_phase0());
    }

    #[test]
    fn flags_propagate() {
        // layout=0, version=0, flags = 0xABC
        let f = InodeFormat::parse(0xABC0).unwrap();
        assert_eq!(f.flags, 0xABC);
    }
}
