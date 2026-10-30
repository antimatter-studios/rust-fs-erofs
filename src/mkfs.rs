//! EROFS image builder.
//!
//! Writes every layout the reader supports EXCEPT compressed data —
//! the one place a scope note is still accurate, and it is about the
//! writer alone. The reader decodes LZ4, LZMA and DEFLATE. Emits compact + extended inodes; FLAT_PLAIN and
//! FLAT_INLINE for regular files; multi-block directories; chunked
//! files (compact + indexed chunkmap, with hole sentinels); inline
//! xattrs (with POSIX-ACL helper); special files (chr/blk/fifo/sock).
//!
//! Layout (general shape, exact addresses computed by the planner):
//!
//! ```text
//! block 0 ..:        SB area (zeros + 128-byte SB at offset 1024, then
//!                    the COMPR_CFGS blob and the xattr prefix dictionary)
//! data area:         shared xattrs, file blocks, symlink target blocks,
//!                    chunked-file chunk blocks, compressed pclusters.
//! meta_blkaddr ..:   metadata area: 32-byte inode slots, optional inline
//!                    xattrs, optional inline FLAT_INLINE tail, optional
//!                    inline chunkmap or compression index.
//! after it:          directory blocks.
//! ```
//!
//! The data comes first so that it can be written in one pass, as each
//! file is read: an inode records where its data is, and the shape of a
//! compressed inode is not known until its data has been compressed. The
//! metadata area follows, once every address is known; NIDs count from
//! `meta_blkaddr`, so the root is still NID 0. See [`build_image_to`].
//!
//! Independent implementation: written from `linux/fs/erofs/erofs_fs.h`
//! (struct definitions only) plus
//! `linux/include/uapi/linux/{stat,posix_acl_xattr,kdev_t}.h`. NOT
//! derived from the Linux EROFS C implementation (`fs/erofs/*.c`,
//! GPL-2) and NOT derived from erofs-utils (BSD-2 / GPL-2 dual).

use crate::acl::{POSIX_ACL_ENTRY_SIZE, POSIX_ACL_HEADER_SIZE, POSIX_ACL_XATTR_VERSION};
use crate::chunked::{
    EROFS_CHUNK_FORMAT_BLKBITS_MASK, EROFS_CHUNK_FORMAT_INDEXES, EROFS_NULL_ADDR,
};
use crate::dir::{ftype, EROFS_DIRENT_SIZE};
use crate::error::{Error, Result};
use crate::inode::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, S_IFSOCK};
use crate::layout::DataLayout;
use crate::lz4hc::Lz4Hc;
pub use crate::superblock::LzmaCfg;
use crate::superblock::{
    EROFS_FEATURE_COMPAT_SB_CHKSUM, EROFS_FEATURE_INCOMPAT_COMPR_CFGS,
    EROFS_FEATURE_INCOMPAT_ZERO_PADDING, EROFS_SUPER_BLOCK_SIZE, EROFS_SUPER_MAGIC_V1,
    EROFS_SUPER_OFFSET,
};
pub use crate::xattr::XattrLongPrefix;
use crate::xattr::{XATTR_ENTRY_ALIGN, XATTR_HEADER_SIZE};
use crate::zmap::{
    CompactPackShape, COMPACT_2B, COMPACT_4B, COMPACT_MAP_EBASE_ALIGN,
    Z_EROFS_ADVISE_BIG_PCLUSTER_1, Z_EROFS_ADVISE_COMPACTED_2B, Z_EROFS_ADVISE_INLINE_PCLUSTER,
    Z_EROFS_COMPACT_MAP_HEADER_SIZE, Z_EROFS_LCLUSTER_INDEX_SIZE, Z_EROFS_LCLUSTER_TYPE_HEAD1,
    Z_EROFS_LCLUSTER_TYPE_NONHEAD, Z_EROFS_LCLUSTER_TYPE_PLAIN, Z_EROFS_LEGACY_MAP_HEADER_SIZE,
    Z_EROFS_LI_D0_CBLKCNT,
};
use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};

// --- public API --------------------------------------------------------

/// In-memory representation of a tree to format. `BTreeMap` for
/// `Dir::entries` so the produced image is deterministic given the same
/// input.
#[derive(Debug)]
pub enum Node {
    File {
        mode: u16,
        data: Vec<u8>,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
    Dir {
        mode: u16,
        entries: BTreeMap<String, Node>,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
    Symlink {
        mode: u16,
        target: String,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
    /// Character or block device. `mode` carries S_IFCHR or S_IFBLK; `rdev`
    /// is the encoded Linux "new" 32-bit dev_t.
    Device {
        mode: u16,
        rdev: u32,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
    /// FIFO (S_IFIFO) or socket (S_IFSOCK). No data, no rdev.
    Special {
        mode: u16,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
    /// Chunk-based regular file. `chunk_bits` selects chunk_size =
    /// block_size << chunk_bits (low 5 bits per the layout-flags field).
    /// Each entry in `chunks` is one chunk's bytes; `None` is a hole
    /// that reads back as zeros. `use_indexed_format` picks the 8-byte
    /// `erofs_inode_chunk_index` map vs. the 4-byte compact map.
    ChunkedFile {
        mode: u16,
        chunk_bits: u8,
        chunks: Vec<Option<Vec<u8>>>,
        use_indexed_format: bool,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
    /// File whose data is compressed at write time. Each lcluster
    /// (size = `block_size << lclusterbits`, default 0 → block_size)
    /// compresses independently into its own pcluster (one pcluster per
    /// lcluster — both W2a (legacy) and W2b (compacted-2B) default
    /// policy: no multi-lcluster collation).
    ///
    /// Two on-disk index encodings selectable via
    /// [`CompressedFileSpec::index_format`]:
    /// - [`CompressedIndexFormat::Legacy`][]: 8-byte-per-lcluster
    ///   `z_erofs_lcluster_index`, datalayout = `CompressionLegacy` (1).
    /// - [`CompressedIndexFormat::Compacted2B`][]: 32-byte-aligned packs
    ///   with bit-packed entries + per-pack base blkaddr trailer,
    ///   datalayout = `Compression` (3). Modern mkfs.erofs default
    ///   since 1.5.
    ///
    /// Optional [`CompressedFileSpec::ztailpacking`] inlines the LAST
    /// pcluster's compressed bytes immediately after the index area
    /// (requires the inline tail + idata_size to fit alongside the
    /// index in the metadata block).
    ///
    /// Scope: LZ4 only; one-block pclusters; no fragments;
    /// no BIG_PCLUSTER. Multi-lcluster collation (one frame spanning
    /// many lclusters) is still future work.
    CompressedFile(CompressedFileSpec),
    /// Another name for a node elsewhere in the tree: a hard link. The
    /// directory entry names the same inode as `target`, a path from the
    /// root (`a/b` or `/a/b`), and that inode's `nlink` counts every
    /// name. The target must not be a directory or another link -- a
    /// tar link entry names the file it links to, never a link.
    HardLink { target: String },
    /// A regular file whose bytes come from a reader, opened when the
    /// writer reaches it and read once, straight into the image. Laid out
    /// exactly as a [`Node::File`] holding the same bytes would be.
    StreamedFile {
        mode: u16,
        content: Content,
        meta: NodeMeta,
        xattrs: Vec<XattrSpec>,
    },
}

/// A regular file's content: its size, known up front, and a way to open
/// a reader over it.
///
/// The writer calls `open` once, when it reaches the file, and reads
/// exactly `size` bytes; a reader that ends early, or has more to give,
/// fails the build rather than writing a file of the wrong length. The
/// file's bytes go to the image as they are read, so content opened
/// lazily -- from another image, or a file on disk -- is never all in
/// memory at once.
pub struct Content {
    size: u64,
    open: Box<dyn FnOnce() -> std::io::Result<Box<dyn Read + Send>> + Send>,
}

impl Content {
    /// Content of `size` bytes, read from what `open` returns.
    pub fn new<R, F>(size: u64, open: F) -> Self
    where
        R: Read + Send + 'static,
        F: FnOnce() -> std::io::Result<R> + Send + 'static,
    {
        Self {
            size,
            open: Box::new(move || open().map(|r| Box::new(r) as Box<dyn Read + Send>)),
        }
    }

    /// Content already in memory.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let size = bytes.len() as u64;
        Self::new(size, move || Ok(std::io::Cursor::new(bytes)))
    }

    /// The content's length in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Every byte, in memory. Refuses content that is not exactly
    /// `size` bytes long.
    fn read_all(self) -> Result<Vec<u8>> {
        let size = self.size;
        let mut reader = (self.open)().map_err(io_error)?;
        let len = usize::try_from(size).map_err(|_| Error::BadInode("file content too large"))?;
        let mut bytes = vec![0u8; len];
        reader.read_exact(&mut bytes).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::BadInode("file content is shorter than its size")
            } else {
                io_error(e)
            }
        })?;
        if reader.read(&mut [0u8; 1]).map_err(io_error)? != 0 {
            return Err(Error::BadInode("file content is longer than its size"));
        }
        Ok(bytes)
    }
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Content")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// An I/O error from a content reader or the image sink, as this crate's
/// error.
fn io_error(e: std::io::Error) -> Error {
    Error::Block(fs_core::Error::Io(e))
}

/// Compression algorithm selector for [`Node::CompressedFile`].
///
/// Algorithm IDs match the EROFS spec's `algorithm_type` byte in the
/// per-inode zmap header:
/// - `Lz4 = 0`: raw LZ4 block, RIGHT-aligned in its pcluster with
///   leading zero pad (`EROFS_FEATURE_INCOMPAT_ZERO_PADDING`).
/// - `Lzma = 1`: LZMA1 stream with the standard 13-byte
///   properties+unpacked_size header that `lzma_rs::lzma_compress`
///   emits. Placed at offset 0 of the pcluster, trailing zero pad.
/// - `Deflate = 2`: raw DEFLATE block (no zlib/gzip wrapper). Placed
///   at offset 0 of the pcluster, trailing zero pad.
///
/// Spec: `linux/fs/erofs/erofs_fs.h::Z_EROFS_COMPRESSION_*`. Independent
/// implementation; not derived from the GPL-2 kernel encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressedAlgo {
    /// LZ4 raw block, with leading zero-padding within the pcluster
    /// (`EROFS_FEATURE_INCOMPAT_ZERO_PADDING`).
    Lz4,
    /// LZMA1 stream (the `lzma_rs::lzma_compress` default output, which
    /// is properties + 8-byte unpacked_size + arithmetic-coded payload
    /// + end marker). NOT `.xz` framed.
    Lzma,
    /// Raw DEFLATE block (no zlib header, no gzip header). What
    /// `flate2::Compress::new(level, /* zlib_header */ false)` emits.
    Deflate,
}

/// On-disk encoding of the per-lcluster index for a compressed inode.
///
/// Spec: `linux/fs/erofs/erofs_fs.h::EROFS_INODE_COMPRESSED_FULL` (1)
/// vs `EROFS_INODE_COMPRESSED_COMPACT` (3). Independent implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressedIndexFormat {
    /// Legacy / uncompacted 8-byte-per-lcluster `z_erofs_lcluster_index`.
    /// Inode datalayout = 1 (`CompressionLegacy`). 16-byte combined
    /// header (8-byte struct header + 8-byte reserved gap), then a
    /// packed array of 8-byte entries. W2a default; preserves W2a
    /// behaviour when the caller doesn't specify [`Self::Compacted2B`].
    Legacy,
    /// Compacted-2B / compacted-4B mixed packs. Inode datalayout = 3
    /// (`Compression`). 8-byte struct header + 32-byte-aligned packs
    /// (each pack: bit-packed entries followed by a `__le32` per-pack
    /// base blkaddr). Modern `mkfs.erofs` default since 1.5.
    Compacted2B,
}

/// Spec for a [`Node::CompressedFile`].
#[derive(Debug, Clone)]
pub struct CompressedFileSpec {
    pub mode: u16,
    pub data: Vec<u8>,
    pub algo: CompressedAlgo,
    /// Logical-cluster size = `block_size << lclusterbits`. Only the
    /// low 4 bits are meaningful (they live in `i_format`'s flag
    /// nibble). 0 → lcluster_size == block_size, the simplest case.
    pub lclusterbits: u8,
    pub meta: NodeMeta,
    pub xattrs: Vec<XattrSpec>,
    /// Index encoding format. Defaults to [`CompressedIndexFormat::Legacy`]
    /// via [`CompressedFileSpec::default_index_format`] for backwards
    /// compatibility with W2a callers.
    pub index_format: CompressedIndexFormat,
    /// When `true` AND the encoder can fit the last lcluster's
    /// compressed bytes inside the remaining metadata-block budget after
    /// the index area, emit them inline (no separate pcluster block).
    /// Sets `Z_EROFS_ADVISE_INLINE_PCLUSTER` on the zmap header, with
    /// `h_idata_size` = compressed length of the last lcluster.
    /// When `false`: every lcluster gets its own pcluster block.
    pub ztailpacking: bool,
    /// Greedy multi-lcluster pcluster-collation budget. When `> 1`, the
    /// writer attempts to collate contiguous lclusters into a shared
    /// pcluster of at most `target_pcluster_blocks * block_size` bytes
    /// of compressed payload before zero-padding. With the EROFS
    /// one-block-pcluster constraint (no `BIG_PCLUSTER` plumbing yet)
    /// the only meaningful values today are 0 (== treat as 1) and 1.
    /// Field is exposed so future BIG_PCLUSTER work can lift the cap
    /// without an API change.
    ///
    /// Spec: pcluster source-byte-range derivation
    /// (`[head*lcsize+head.clusterofs, next_head*lcsize+next.clusterofs)`)
    /// described in the public EROFS compression-format documentation
    /// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
    pub target_pcluster_blocks: u32,
}

impl CompressedFileSpec {
    /// Default index format choice when callers don't specify one.
    /// Kept separate from `Default::default` so adding the field is a
    /// purely additive API change for W2a callers.
    pub const fn default_index_format() -> CompressedIndexFormat {
        CompressedIndexFormat::Legacy
    }

    /// Default `target_pcluster_blocks` for new callers. 1 block is the
    /// only currently-supported value (no BIG_PCLUSTER); the greedy
    /// collator still gets to absorb any number of lclusters whose
    /// COMBINED LZ4 frame fits in 1 block, which is the common case for
    /// highly compressible data and the win this default delivers.
    pub const fn default_target_pcluster_blocks() -> u32 {
        1
    }
}

/// Optional inode metadata. Default-zero; setting any non-default field
/// promotes the on-disk shape to 64-byte extended.
#[derive(Debug, Default, Clone, Copy)]
pub struct NodeMeta {
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub mtime_nsec: u32,
}

/// One xattr entry: namespace index, name (after the namespace prefix),
/// raw value bytes. Empty `name` is valid for POSIX-ACL slots
/// (`name_index` 2 or 3) where the full name is implicit.
#[derive(Debug, Clone)]
pub struct XattrSpec {
    pub name_index: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl XattrSpec {
    pub fn new(name_index: u8, name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            name_index,
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Default mode bits, kept for backwards compatibility with tests that
/// only set the type+permissions and don't care about uid/gid/mtime.
pub const DEFAULT_DIR_MODE: u16 = 0o040755;
pub const DEFAULT_FILE_MODE: u16 = 0o100644;
pub const DEFAULT_SYMLINK_MODE: u16 = 0o120777;

/// Optional per-image build configuration. Default-empty: no custom
/// xattr prefix dictionary, no COMPR_CFGS blob. Construct via
/// [`BuildOptions::default`] and set fields as needed; pass to
/// [`build_image_with`].
///
/// Both fields are additive — leaving them at default produces
/// byte-identical output to the legacy [`build_image`] entry point.
#[derive(Debug, Default, Clone)]
pub struct BuildOptions {
    /// Custom xattr name prefixes. When non-empty, the writer emits a
    /// dictionary in the pre-metadata gap between the superblock and
    /// the first metadata block, and points `sb.xattr_prefix_*` at it.
    /// Each entry is `(base_namespace_index, infix bytes)`. Inline
    /// xattrs that use the custom-prefix flag bit (`0x80` in
    /// `e_name_index`) reference `dict[name_index & 0x7F]`.
    ///
    /// Spec: `linux/fs/erofs/xattr.h::erofs_xattr_long_prefix`. The
    /// reader's [`crate::xattr::resolve_with_dict`] consumes the same
    /// format. Independent implementation.
    pub xattr_prefixes: Vec<XattrLongPrefix>,

    /// When `Some`, the writer emits a COMPR_CFGS blob right after the
    /// 128-byte superblock and sets the
    /// `EROFS_FEATURE_INCOMPAT_COMPR_CFGS` bit. Reader decoders consult
    /// this for non-default codec props (today: LZMA `dict_size`).
    pub compr_cfgs: Option<ComprCfgsConfig>,

    /// The superblock's 16-byte volume UUID. All zeros by default, which
    /// tools read as "no UUID".
    pub uuid: [u8; 16],

    /// The superblock's `build_time`, in seconds since the epoch, and
    /// its nanoseconds. A compact inode has no mtime of its own and
    /// reads as this, so an inode whose mtime equals it stays compact.
    pub build_time: u64,
    pub build_time_nsec: u32,

    /// When `true`, every inode's mtime is `build_time` whatever its
    /// [`NodeMeta`] says -- what `mkfs.erofs -T` does. With a fixed
    /// `build_time` the same tree then builds to the same bytes, and
    /// every inode that needs nothing wider is compact.
    pub mtime_is_build_time: bool,

    /// When `true`, an xattr -- the same namespace, name and value -- that
    /// two or more inodes carry is written once, in the shared xattr area,
    /// and each of those inodes holds a 4-byte reference to it in place
    /// of the entry. `mkfs.erofs` shares repeated xattrs the same way; an
    /// SELinux label carried by thousands of inodes costs four bytes each.
    pub share_xattrs: bool,

    /// When `Some`, every regular file -- [`Node::File`] and
    /// [`Node::StreamedFile`] -- is compressed as `mkfs.erofs
    /// -zlz4hc,LEVEL -C BYTES` compresses it. `None` stores them as they
    /// are. [`Node::CompressedFile`] keeps the codec and shape its own
    /// spec names either way.
    pub compression: Option<Compression>,
}

/// How [`BuildOptions::compression`] compresses regular files.
///
/// Each physical cluster holds as much of the file as LZ4HC can fit in
/// `max_pcluster_bytes` -- a big pcluster when that is more than one
/// block -- and the logical clusters are one block each. A pcluster that
/// would save no block is stored as it is, one block of the file in one
/// block of the image; a file where that is true of every pcluster is
/// stored uncompressed, as a file that was never compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compression {
    /// The LZ4HC level, 1..=12, as `-zlz4hc,LEVEL`. 12 is the highest
    /// ratio LZ4 reaches. Every level writes standard LZ4 blocks.
    pub lz4hc_level: u8,
    /// The most compressed bytes one pcluster holds, as `-C`: a
    /// multiple of the block size, at most 64 blocks.
    pub max_pcluster_bytes: u32,
}

impl Compression {
    /// Refuse a level LZ4HC does not have, and a pcluster size that is
    /// not a whole number of blocks from one block to 1 MiB, the most
    /// the kernel decompresses at once.
    fn validate(&self, bs: u64) -> Result<()> {
        if !(1..=12).contains(&self.lz4hc_level) {
            return Err(Error::BadInode("LZ4HC level outside 1..=12"));
        }
        let max = self.max_pcluster_bytes as u64;
        if max < bs || !max.is_multiple_of(bs) || max > 1 << 20 {
            return Err(Error::BadInode(
                "max_pcluster_bytes is not a whole number of blocks up to 1 MiB",
            ));
        }
        Ok(())
    }

    /// `-zlz4hc,12 -C 32768`.
    pub const LZ4HC_12_32K: Compression = Compression {
        lz4hc_level: 12,
        max_pcluster_bytes: 32 << 10,
    };
}

/// Per-codec configuration to surface in the COMPR_CFGS blob.
///
/// Each field selects whether its codec record is emitted. When a
/// codec is `None` the corresponding bit in the SB's
/// `available_compr_algs` field (`u1`) is left clear and no record is
/// written; when `Some(_)`, the bit is set and a single record is
/// emitted in canonical codec order (LZ4, LZMA, DEFLATE).
///
/// Spec: blob layout in the public EROFS on-disk-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html>); per-codec
/// struct field names from the public `erofs_fs.h` constants.
/// Independent implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct ComprCfgsConfig {
    /// LZ4 record. Payload is `__le16 max_distance; __le16
    /// max_pcluster_blks; u8 reserved[10]` (14 bytes, as erofs-utils
    /// writes it). `Some(max_distance)` sets `max_distance`;
    /// `max_pcluster_blks` is the largest pcluster the image holds.
    pub lz4: Option<u16>,
    /// LZMA record. Payload is `__le32 dict_size; __le16 format; u8
    /// reserved[8];` (14 bytes). Only `dict_size` is propagated to the
    /// codec; `format`/`reserved` are written as zero.
    pub lzma: Option<LzmaCfg>,
    /// DEFLATE record. Payload is `u8 windowbits; u8 reserved[5];`
    /// (6 bytes). The reader's DEFLATE codec doesn't honour
    /// `windowbits`, but the bit advertisement is still correct.
    pub deflate: Option<u8>,
}

use crate::inode::compact_offsets as ci;
use crate::inode::extended_offsets as xi;
use crate::inode::{COMPACT_INODE_SIZE, EXTENDED_INODE_SIZE};
/// First byte past the superblock, and so the first byte the metadata
/// area may use.
const SB_AREA_END: u64 = EROFS_SUPER_OFFSET + EROFS_SUPER_BLOCK_SIZE as u64;

/// Encode a POSIX ACL value. Returns the wire bytes you stuff into a
/// `XattrSpec.value` with `name_index` = 2 (access) or 3 (default).
///
/// Spec: `linux/include/uapi/linux/posix_acl_xattr.h`. Independent
/// implementation.
pub fn encode_posix_acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(POSIX_ACL_HEADER_SIZE + entries.len() * POSIX_ACL_ENTRY_SIZE);
    out.extend_from_slice(&POSIX_ACL_XATTR_VERSION.to_le_bytes());
    for (tag, perm, id) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&perm.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

/// Build an EROFS image as an in-memory byte buffer using default
/// build options (no custom xattr prefix dict, no COMPR_CFGS blob).
///
/// `blkszbits` is `log2(block_size)`. 12 (4 KiB) is conventional; 9..=16
/// covers the spec's 512 B .. 64 KiB range.
///
/// Calls [`build_image_with`] with `BuildOptions::default()`. Output is
/// byte-identical to pre-`BuildOptions` callers — no behaviour change
/// for tests and consumers that don't supply options.
pub fn build_image(root: Node, blkszbits: u8) -> Result<Vec<u8>> {
    build_image_with(root, blkszbits, BuildOptions::default())
}

/// Build an EROFS image with extended writer options. See
/// [`BuildOptions`] for the per-image knobs.
///
/// When `options.xattr_prefixes` is non-empty, the writer emits the
/// dictionary in the gap between the SB area and the first metadata
/// block, and the SB's `xattr_prefix_*` fields point at it. When
/// `options.compr_cfgs` is `Some(_)`, the writer emits the COMPR_CFGS
/// blob immediately after the 128-byte SB, sets
/// `EROFS_FEATURE_INCOMPAT_COMPR_CFGS`, and advertises the included
/// codecs in the SB's `available_compr_algs` (`u1`) field.
pub fn build_image_with(root: Node, blkszbits: u8, options: BuildOptions) -> Result<Vec<u8>> {
    let mut image = std::io::Cursor::new(Vec::new());
    build_image_to(root, blkszbits, options, &mut image)?;
    Ok(image.into_inner())
}

/// Build an EROFS image into `sink`, and return its length in bytes.
///
/// The one implementation of the layout: [`build_image_with`] is this
/// writing into a `Vec`. File data is written first, from the block after
/// the superblock, as each file's [`Content`] is read; the metadata area
/// and the directories follow it, and the superblock is written last. So
/// the sink is written out of order, which is what `Seek` is for, and
/// memory holds the tree's metadata -- inodes, xattrs, inline tails,
/// compression indexes, directory entries -- but never its file data.
///
/// The sink is written from its start; anything already past the image's
/// end is left as it was. The same tree and options give the same bytes.
pub fn build_image_to<W: Write + Seek>(
    root: Node,
    blkszbits: u8,
    options: BuildOptions,
    sink: &mut W,
) -> Result<u64> {
    if !crate::superblock::is_valid_blkszbits(blkszbits) {
        return Err(Error::BadSuperblock("blkszbits out of range"));
    }
    let bs: u64 = 1u64 << blkszbits;

    // Pass 1: flatten + per-inode body planning (size, xattrs, inline tail,
    // chunkmap). Mutates the tree in place to record FlatInline vs FlatPlain
    // file layout.
    let mut plan: Vec<PlanNode> = Vec::new();
    let mut links: Vec<PendingLink> = Vec::new();
    flatten(root, 0, &mut plan, &mut links)?;
    plan[0].parent_idx = 0;
    resolve_hard_links(&mut plan, links)?;
    let nlinks = link_counts(&plan);
    let shared = if options.share_xattrs {
        share_repeated_xattrs(&mut plan)?
    } else {
        SharedXattrs::none(plan.len())
    };
    if options.mtime_is_build_time {
        for node in &mut plan {
            node.meta.mtime = options.build_time;
            node.meta.mtime_nsec = options.build_time_nsec;
        }
    }

    // Detect which non-LZ4 codecs the tree actually uses. LZ4
    // (algorithmtype 0) is the implicit default and needs no
    // declaration; LZMA / DEFLATE / ZSTD MUST be advertised in
    // `available_compr_algs` + the COMPR_CFGS blob, or fsck.erofs
    // rejects the image with "inconsistent algorithmtype N". This drives
    // automatic COMPR_CFGS emission even when the caller didn't supply
    // an explicit `options.compr_cfgs`.
    let mut used_lzma = false;
    let mut used_deflate = false;
    for node in &plan {
        if let PlanKind::Compressed { algo, .. } = &node.kind {
            match algo {
                CompressedAlgo::Lz4 => {}
                CompressedAlgo::Lzma => used_lzma = true,
                CompressedAlgo::Deflate => used_deflate = true,
            }
        }
    }

    // The COMPR_CFGS blob (when present) influences how the LZMA encoder
    // synthesises its on-disk header — we patch the in-stream
    // `dict_size` field to match what we'll advertise in the blob,
    // because lzma-rs has no knob for it. Other codecs ignore the
    // override. An explicit `options.compr_cfgs` LZMA dict_size wins;
    // otherwise, when LZMA is auto-detected, we pin both the in-stream
    // header and the advertised cfg to the LZMA1 default (1 << 24) so
    // the two always agree.
    let lzma_dict_size_override: Option<u32> = options
        .compr_cfgs
        .as_ref()
        .and_then(|c| c.lzma.as_ref())
        .map(|cfg| cfg.dict_size)
        .or(if used_lzma {
            Some(LzmaCfg::default().dict_size)
        } else {
            None
        });

    let mut bodies: Vec<InodeBody> = Vec::with_capacity(plan.len());
    for ((node, &nlink), refs) in plan.iter_mut().zip(&nlinks).zip(&shared.refs) {
        let body = plan_body(node, nlink, refs, bs, lzma_dict_size_override, &options)?;
        bodies.push(body);
    }

    // Pass 2: the data area, streamed. It starts at the first block past
    // the superblock area, and every file's bytes go to the sink as they
    // are read, so nothing holds more than one copy buffer of them. The
    // block(s) before it are zeroed now and written last, once the
    // superblock knows where everything went.
    let data_start = SB_AREA_END.div_ceil(bs);
    let mut out = ImageSink::new(sink, bs);
    out.write_blocks(0, &[], data_start)?;
    let mut next_data_block = data_start;
    // The shared xattr area leads the data area: it is addressed by
    // block, and its entries by 4-byte offsets into it.
    let xattr_blkaddr: u32 = if shared.area.is_empty() {
        0
    } else {
        let at = block_addr(next_data_block)?;
        let blocks = (shared.area.len() as u64).div_ceil(bs);
        out.write_blocks(next_data_block, &shared.area, blocks)?;
        next_data_block += blocks;
        at
    };
    let mut placements: Vec<Placement> = Vec::with_capacity(plan.len());
    let mut any_compressed = false;
    let mut any_streamed_compressed = false;
    let mut lz4hc = options
        .compression
        .map(|c| c.validate(bs).map(|()| (Lz4Hc::new(c.lz4hc_level), c)))
        .transpose()?;
    for (n, body) in plan.iter_mut().zip(bodies.iter_mut()) {
        let mut place = Placement::default();
        let mut compressed: Option<PlanKind> = None;
        match &mut n.kind {
            PlanKind::File {
                content,
                size,
                layout,
            } if *layout == FileLayout::FlatPlain && *size > 0 => {
                let content = content.take().expect("a file's content is read once");
                let start = next_data_block;
                // The last partial block goes inline when it fits after
                // the inode, and the file takes only its whole blocks.
                let tail = *size % bs;
                let inline = tail > 0 && tail <= tail_room(body, bs);
                // A file of a block or less cannot be stored in fewer
                // blocks, so it is never compressed.
                let streamed = match &mut lz4hc {
                    Some((encoder, compression)) if *size > bs => compress_streamed(
                        &mut out,
                        encoder,
                        content,
                        start,
                        compression.max_pcluster_bytes as usize,
                    )?,
                    _ => Streamed::Stored {
                        last: out.write_content(start, content, if inline { tail } else { 0 })?,
                    },
                };
                match streamed {
                    Streamed::Stored { last } => {
                        place.blkaddr = start;
                        if inline {
                            // A stored file's last partial block was written
                            // to the sink; the next data overwrites it.
                            *layout = FileLayout::FlatInline;
                            body.inline_tail = last;
                            next_data_block += *size / bs;
                        } else {
                            next_data_block += size.div_ceil(bs);
                        }
                    }
                    Streamed::Compressed {
                        pclusters,
                        entries,
                        addrs,
                        blocks,
                    } => {
                        any_compressed = true;
                        any_streamed_compressed = true;
                        let (_, compression) = lz4hc.as_ref().expect("compressed with it");
                        // The legacy map header sits at the 8-byte boundary
                        // after the inode body and its xattrs; every slot
                        // starts 32-byte aligned, so that boundary is known
                        // from their sizes alone.
                        let end = body.body_size + body.xattr_bytes.len() as u64;
                        body.zmap_bytes = end.next_multiple_of(8) - end
                            + Z_EROFS_LEGACY_MAP_HEADER_SIZE
                            + entries.len() as u64 * Z_EROFS_LCLUSTER_INDEX_SIZE;
                        place.pcluster_addrs = addrs;
                        next_data_block += blocks;
                        compressed = Some(PlanKind::Compressed {
                            src: Vec::new(),
                            size: *size,
                            big_pcluster: compression.max_pcluster_bytes as u64 > bs,
                            algo: CompressedAlgo::Lz4,
                            lclusterbits: 0,
                            pclusters,
                            lcluster_entries: entries,
                            total_compressed_blocks: u32::try_from(blocks)
                                .map_err(|_| Error::BadInode("compressed blocks overflow u32"))?,
                            index_format: CompressedIndexFormat::Legacy,
                            ztailpacking_requested: false,
                            ztailpacking_inline: None,
                            target_pcluster_blocks: (compression.max_pcluster_bytes as u64 / bs)
                                as u32,
                        });
                    }
                }
            }
            PlanKind::File { content, .. } => {
                // An empty file still opens its content, to refuse a
                // reader with bytes behind a size of zero.
                if let Some(content) = content.take() {
                    content.read_all()?;
                }
            }
            PlanKind::Symlink { target } => {
                if target.len() as u64 > bs {
                    return Err(Error::BadInode("symlink target longer than one block"));
                }
                if !target.is_empty() && target.len() as u64 <= tail_room(body, bs) {
                    body.inline_tail = target.clone();
                } else {
                    place.blkaddr = next_data_block;
                    out.write_blocks(next_data_block, target, 1)?;
                    next_data_block += 1;
                }
            }
            PlanKind::Chunked { chunks, .. } => {
                for c in chunks.iter() {
                    match c {
                        None => place.chunk_addrs.push(EROFS_NULL_ADDR),
                        Some(buf) => {
                            let blocks = (buf.len() as u64).div_ceil(bs).max(1);
                            place.chunk_addrs.push(block_addr(next_data_block)?);
                            out.write_blocks(next_data_block, buf, blocks)?;
                            next_data_block += blocks;
                        }
                    }
                }
            }
            PlanKind::Compressed { pclusters, .. } => {
                any_compressed = true;
                for pc in pclusters.iter_mut() {
                    // For ztailpacked pclusters (block_count == 0) we
                    // still record next_data_block as the entry's
                    // blkaddr. The reader resolves this via the per-
                    // pack base arithmetic but uses the inline-tail
                    // offset/size for actual reads (gated on
                    // `is_last_pcluster && has_inline_tail`), so the
                    // recorded address is a "shadow" that keeps
                    // compacted-2B per-pack base math consistent for
                    // non-tail entries in the same pack.
                    place.pcluster_addrs.push(block_addr(next_data_block)?);
                    let blocks = pc.pcluster_block_count as u64;
                    if blocks > 0 {
                        out.write_blocks(
                            next_data_block,
                            &std::mem::take(&mut pc.block_bytes),
                            blocks,
                        )?;
                    }
                    next_data_block += blocks;
                }
            }
            PlanKind::Dir { .. } | PlanKind::Device { .. } | PlanKind::Special => {}
        }
        if let Some(kind) = compressed {
            n.kind = kind;
        }
        placements.push(place);
    }

    // A directory's last block goes inline when it fits after the inode.
    // Which entries fill which block depends only on their names, so the
    // blocks are sized here, before NIDs exist, with zeros for them; the
    // inline tail is a placeholder of the right length until pass 4
    // encodes the real entries.
    let no_nids = vec![0u64; plan.len()];
    for (i, n) in plan.iter().enumerate() {
        if let PlanKind::Dir { children } = &n.kind {
            let (blocks, last_used) = encode_dir_blocks(i, children, &plan, &no_nids, bs)?;
            let full = blocks.len() as u64 - 1;
            if last_used as u64 <= tail_room(&bodies[i], bs) {
                bodies[i].inline_tail = vec![0u8; last_used];
                placements[i].dir_size = full * bs + last_used as u64;
            } else {
                placements[i].dir_size = (full + 1) * bs;
            }
        }
    }

    // Pass 3: lay out the metadata area, right after the data. Each inode
    // body + its trailers (xattrs, inline tail, chunkmap, zmap) consumes
    // contiguous bytes; `plan_meta_layout` owns the placement rules and is
    // the single walk of that area. The NIDs below and the write addresses
    // used by the emission loop further down are both read out of the same
    // slots. NIDs count from `meta_blkaddr`, so the root, planned first,
    // is NID 0 however much data precedes it.
    let meta_blkaddr =
        block_addr(next_data_block).map_err(|_| Error::BadSuperblock("meta_blkaddr overflow"))?;
    let meta_byte_base = meta_blkaddr as u64 * bs;

    let meta_slots = plan_meta_layout(&bodies, bs);
    let nids: Vec<u64> = meta_slots.iter().map(MetaSlot::nid).collect();
    let meta_total_bytes = meta_slots
        .last()
        .map_or(0, MetaSlot::end)
        .div_ceil(COMPACT_INODE_SIZE)
        * COMPACT_INODE_SIZE;
    let meta_blocks = meta_total_bytes.div_ceil(bs);

    // Pass 4: each directory's entries, packed into blocks after the
    // metadata area and written as they are packed. The child-NID lookup
    // uses the `nids` map computed above, so this can only run after
    // pass 3; the directory inodes that point here are encoded after it.
    let mut next_block = meta_blkaddr as u64 + meta_blocks;
    for (i, n) in plan.iter().enumerate() {
        if let PlanKind::Dir { children } = &n.kind {
            let (mut blocks, last_used) = encode_dir_blocks(i, children, &plan, &nids, bs)?;
            if !bodies[i].inline_tail.is_empty() {
                let last = blocks.pop().expect("a directory has a block");
                bodies[i].inline_tail = last[..last_used].to_vec();
            }
            if !blocks.is_empty() {
                placements[i].blkaddr = next_block;
            }
            for buf in &blocks {
                out.write_blocks(next_block, buf, 1)?;
                next_block += 1;
            }
        }
    }
    let total_blocks = next_block;

    if total_blocks < 2 {
        return Err(Error::BadSuperblock("image too small"));
    }
    if total_blocks > u32::MAX as u64 {
        return Err(Error::BadSuperblock("FS too large for u32 block count"));
    }

    // Pass 5: the superblock area -- everything before the data -- and
    // the metadata area, both built in memory and written once.
    let mut img = vec![0u8; (data_start * bs) as usize];
    let mut feature_incompat = if any_compressed {
        // Modern fsck.erofs (>= 1.6) requires LZ4_0PADDING for any
        // compressed inode. Setting the bit unconditionally when the
        // image carries compressed data matches what mkfs.erofs has
        // emitted since Linux 5.4.
        EROFS_FEATURE_INCOMPAT_ZERO_PADDING
    } else {
        0
    };

    // Encode COMPR_CFGS blob into the gap right after the SB area.
    // Format: a sequence of `__le16 size; payload[size];` records, one
    // per advertised codec, in canonical order LZ4, LZMA, DEFLATE. The
    // SB's `available_compr_algs` (`u1`) bitmap advertises which
    // records are present; the reader walks codecs in the same order
    // and consumes records exclusively for the bits that are set.
    //
    // Spec: layout described in the public EROFS on-disk-format
    // documentation; per-codec struct layouts from the public
    // `erofs_fs.h` constants. Independent implementation.
    //
    // The set of codecs to declare is the union of (a) the codecs an
    // inode actually references (`used_lzma` / `used_deflate`, detected
    // above) and (b) any codecs the caller pinned via
    // `options.compr_cfgs`. A caller-supplied per-codec record wins for
    // its parameters; auto-detected codecs fall back to the codec
    // defaults that match native `mkfs.erofs` output (LZMA dict_size
    // = 1 << 24, DEFLATE windowbits = 15). LZ4 (algorithmtype 0) is the
    // implicit default and is only declared when the caller explicitly
    // asks for it.
    let cfg = options.compr_cfgs.as_ref();
    // A file compressed as it streams can have pclusters of more than
    // one block, which the LZ4 record's `max_pcluster_blks` must allow:
    // declare LZ4 with liblz4's full 64 KiB - 1 match distance, as
    // `mkfs.erofs` does.
    let lz4_cfg: Option<u16> = cfg
        .and_then(|c| c.lz4)
        .or(any_streamed_compressed.then_some(u16::MAX));
    let lzma_cfg: Option<LzmaCfg> = cfg.and_then(|c| c.lzma).or(if used_lzma {
        Some(LzmaCfg::default())
    } else {
        None
    });
    let deflate_cfg: Option<u8> = cfg.and_then(|c| c.deflate).or(if used_deflate {
        // 15 == max DEFLATE window (32 KiB); what native mkfs.erofs
        // advertises and what flate2's default encoder produces.
        Some(15)
    } else {
        None
    });

    let mut sb_u1: u16 = 0;
    let mut compr_cfgs_bytes: Vec<u8> = Vec::new();
    if lz4_cfg.is_some() || lzma_cfg.is_some() || deflate_cfg.is_some() {
        feature_incompat |= EROFS_FEATURE_INCOMPAT_COMPR_CFGS;
        if let Some(max_distance) = lz4_cfg {
            sb_u1 |= 1 << 0; // Z_EROFS_COMPRESSION_LZ4_BIT
                             // LZ4 record: __le16 max_distance; __le16 max_pcluster_blks;
                             // u8 reserved[10]. FOURTEEN BYTES, as erofs-utils writes it:
                             // the four-byte record this wrote made `fsck.erofs` refuse the
                             // whole image at the superblock ("invalid lz4 cfgs, size=4"),
                             // and its max_pcluster_blks of zero said no pcluster may hold
                             // a block (#57). The count is the largest pcluster this image
                             // wrote, and at least one.
            let max_pcluster_blks = plan
                .iter()
                .filter_map(|node| match &node.kind {
                    PlanKind::Compressed { pclusters, .. } => {
                        pclusters.iter().map(|p| p.pcluster_block_count).max()
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(1)
                .max(1);
            let max_pcluster_blks = u16::try_from(max_pcluster_blks)
                .map_err(|_| Error::BadInode("pcluster too large for the LZ4 cfgs record"))?;
            compr_cfgs_bytes.extend_from_slice(&14u16.to_le_bytes());
            compr_cfgs_bytes.extend_from_slice(&max_distance.to_le_bytes());
            compr_cfgs_bytes.extend_from_slice(&max_pcluster_blks.to_le_bytes());
            compr_cfgs_bytes.extend_from_slice(&[0u8; 10]); // reserved
        }
        if let Some(lzma) = lzma_cfg {
            sb_u1 |= 1 << 1; // Z_EROFS_COMPRESSION_LZMA_BIT
                             // LZMA record: __le32 dict_size; __le16 format; u8 reserved[8];
            compr_cfgs_bytes.extend_from_slice(&14u16.to_le_bytes());
            compr_cfgs_bytes.extend_from_slice(&lzma.dict_size.to_le_bytes());
            compr_cfgs_bytes.extend_from_slice(&0u16.to_le_bytes()); // format
            compr_cfgs_bytes.extend_from_slice(&[0u8; 8]); // reserved
        }
        if let Some(window_bits) = deflate_cfg {
            sb_u1 |= 1 << 2; // Z_EROFS_COMPRESSION_DEFLATE_BIT
                             // DEFLATE record: u8 windowbits; u8 reserved[5];
            compr_cfgs_bytes.extend_from_slice(&6u16.to_le_bytes());
            compr_cfgs_bytes.push(window_bits);
            compr_cfgs_bytes.extend_from_slice(&[0u8; 5]); // reserved
        }
    }

    // Encode the xattr prefix dictionary (if any) into the SB→meta gap.
    // Layout: each entry is `__le16 size; u8 base_index; u8 infix[size-1];`
    // padded to a 4-byte boundary between entries. `xattr_prefix_start`
    // is the byte offset of the dictionary divided by 4 (NOT a block
    // address), per the empirical convention documented in
    // `xattr::read_xattr_prefix_dictionary`.
    let mut xattr_prefix_count: u8 = 0;
    let mut xattr_prefix_start_div4: u32 = 0;
    let mut xattr_prefix_bytes: Vec<u8> = Vec::new();
    if !options.xattr_prefixes.is_empty() {
        if options.xattr_prefixes.len() > u8::MAX as usize {
            return Err(Error::BadXattr("xattr_prefixes count > 255"));
        }
        for prefix in &options.xattr_prefixes {
            // size = 1 (base_index byte) + infix.len(); must fit in u16.
            let size = 1usize
                .checked_add(prefix.infix.len())
                .ok_or(Error::BadXattr("xattr prefix entry size overflow usize"))?;
            if size > u16::MAX as usize {
                return Err(Error::BadXattr("xattr prefix entry size > 65535"));
            }
            xattr_prefix_bytes.extend_from_slice(&(size as u16).to_le_bytes());
            xattr_prefix_bytes.push(prefix.base_index);
            xattr_prefix_bytes.extend_from_slice(&prefix.infix);
            // 4-byte align the cursor before the next entry.
            while !(xattr_prefix_bytes.len() as u64).is_multiple_of(XATTR_ENTRY_ALIGN) {
                xattr_prefix_bytes.push(0);
            }
        }
        xattr_prefix_count = options.xattr_prefixes.len() as u8;
    }

    // Place the COMPR_CFGS blob at byte offset SB_AREA_END (= 1152,
    // immediately after the 128-byte SB; sb_extslots is left at 0).
    // Place the xattr prefix dictionary right after the cfgs blob,
    // 4-byte aligned. Both must fit in the gap before the first
    // metadata block; this is a hard constraint because the dict's
    // start is encoded as a divided-by-4 byte offset in the SB.
    let mut gap_cursor: u64 = SB_AREA_END;
    if !compr_cfgs_bytes.is_empty() {
        let off = gap_cursor as usize;
        let end = off + compr_cfgs_bytes.len();
        if end as u64 > data_start * bs {
            return Err(Error::BadSuperblock(
                "COMPR_CFGS blob overflows pre-meta gap",
            ));
        }
        img[off..end].copy_from_slice(&compr_cfgs_bytes);
        gap_cursor = end as u64;
    }
    if !xattr_prefix_bytes.is_empty() {
        // 4-byte-align the start so the divided-by-4 encoding is exact.
        gap_cursor = gap_cursor.next_multiple_of(XATTR_ENTRY_ALIGN);
        let dict_byte_off = gap_cursor;
        if dict_byte_off / 4 > u32::MAX as u64 {
            return Err(Error::BadXattr("xattr_prefix_start overflows u32"));
        }
        xattr_prefix_start_div4 = (dict_byte_off / 4) as u32;
        let off = dict_byte_off as usize;
        let end = off + xattr_prefix_bytes.len();
        if end as u64 > data_start * bs {
            return Err(Error::BadXattr(
                "xattr prefix dictionary overflows pre-meta gap",
            ));
        }
        img[off..end].copy_from_slice(&xattr_prefix_bytes);
    }

    write_superblock(
        &mut img,
        blkszbits,
        plan.len() as u64,
        total_blocks as u32,
        meta_blkaddr,
        feature_incompat,
        sb_u1,
        xattr_prefix_count,
        xattr_prefix_start_div4,
        xattr_blkaddr,
        &options,
    );
    out.write_at(0, &img)?;

    // Inodes + their inline trailers. Every address here comes out of
    // the pass-2 layout, so an inode's bytes land at exactly the address
    // its recorded NID names — there is no second cursor walk that could
    // fall out of step with the first.
    let mut meta = vec![0u8; (meta_blocks * bs) as usize];
    for (i, (body, slot)) in bodies.iter().zip(&meta_slots).enumerate() {
        let nid = slot.nid();
        let mut meta_cursor = slot.start;
        let inode_off = meta_cursor as usize;
        let inode_buf = encode_inode(&plan[i], nid, body, nlinks[i], &placements[i], bs);
        meta[inode_off..inode_off + body.body_size as usize].copy_from_slice(&inode_buf);
        meta_cursor += body.body_size;

        if !body.xattr_bytes.is_empty() {
            let off = meta_cursor as usize;
            meta[off..off + body.xattr_bytes.len()].copy_from_slice(&body.xattr_bytes);
            meta_cursor += body.xattr_bytes.len() as u64;
        }
        if !body.inline_tail.is_empty() {
            let off = meta_cursor as usize;
            meta[off..off + body.inline_tail.len()].copy_from_slice(&body.inline_tail);
            meta_cursor += body.inline_tail.len() as u64;
        }
        if body.chunkmap_bytes > 0 {
            let off = meta_cursor as usize;
            let map_buf = encode_chunkmap(&plan[i], &placements[i].chunk_addrs);
            if map_buf.len() as u64 != body.chunkmap_bytes {
                return Err(Error::BadInode(
                    "emitted chunkmap length disagrees with the planned trailer size",
                ));
            }
            meta[off..off + map_buf.len()].copy_from_slice(&map_buf);
            meta_cursor += body.chunkmap_bytes;
        }
        if body.zmap_bytes > 0 {
            let off = meta_cursor as usize;
            let body_end = meta_byte_base + meta_cursor;
            let zmap_buf =
                encode_zmap_trailer(&plan[i], &placements[i].pcluster_addrs, body_end, blkszbits);
            if zmap_buf.len() as u64 != body.zmap_bytes {
                return Err(Error::BadInode(
                    "emitted zmap trailer length disagrees with the planned trailer size",
                ));
            }
            meta[off..off + zmap_buf.len()].copy_from_slice(&zmap_buf);
            meta_cursor += body.zmap_bytes;
        }

        // The body and its trailers must have filled the slot exactly.
        // Anything else means the emission above and `body_span` no
        // longer account for the same set of trailers, which would put
        // this inode's tail bytes inside the next inode's slot. Checked
        // for real, not with a `debug_assert!`: CI gates pull requests
        // with `cargo test --release`, where debug assertions are off.
        if meta_cursor != slot.end() {
            return Err(Error::BadInode(
                "inode emission did not fill its planned metadata span",
            ));
        }
    }
    out.write_at(meta_byte_base, &meta)?;

    Ok(total_blocks * bs)
}

/// Where one inode's data went in the image. Recorded while the data is
/// written, before NIDs exist, and read when the inode is encoded.
#[derive(Debug, Default)]
struct Placement {
    /// First block of a FLAT_PLAIN file, a symlink or a directory; zero
    /// for an inode with no data blocks.
    blkaddr: u64,
    /// A directory's size: its blocks, in bytes.
    dir_size: u64,
    /// A chunked file's chunks, each its block or the hole sentinel.
    chunk_addrs: Vec<u32>,
    /// A compressed file's pclusters, each its first block.
    pcluster_addrs: Vec<u32>,
}

/// What compressing one file as it was read came to.
enum Streamed {
    /// Nothing in it was worth compressing: its bytes are in the image as
    /// they were read, from its first block on, as a FLAT_PLAIN file's
    /// are. `last` is its last partial block, if it ends in one, for an
    /// inline tail.
    Stored { last: Vec<u8> },
    /// Its pclusters, one index entry per lcluster, each pcluster's first
    /// block, and the blocks they fill.
    Compressed {
        pclusters: Vec<PclusterPlan>,
        entries: Vec<LclusterIndexEntry>,
        addrs: Vec<u32>,
        blocks: u64,
    },
}

/// The most of a file LZ4HC is shown at once. A pcluster holds what
/// compresses into it from the start of this, so it bounds how much one
/// pcluster can hold; at 1 MiB a block of the image still holds a whole
/// pcluster of a file's zeros.
const COMPRESS_WINDOW: usize = 1 << 20;

/// Compress a file as it is read, into pclusters written from block
/// `first_block` on, as `mkfs.erofs -zlz4hc -C` does.
///
/// Each pcluster takes as much of the file as LZ4HC fits in
/// `max_pcluster_bytes`, padded at the front to whole blocks
/// (`EROFS_FEATURE_INCOMPAT_ZERO_PADDING`) -- unless that saves no block
/// over the bytes it took, when one block of the file is stored as it is
/// (a PLAIN pcluster) instead. Pclusters need not start on a block, so a
/// head lcluster records where in it its pcluster starts. Logical clusters
/// are one block each.
///
/// When every pcluster is PLAIN, each one holds the next block of the file
/// from offset 0, so the file is in the image exactly as an uncompressed
/// file is: [`Streamed::Stored`] says to describe it as one, and nothing
/// is read twice.
fn compress_streamed<W: Write + Seek>(
    out: &mut ImageSink<'_, W>,
    encoder: &mut Lz4Hc,
    content: Content,
    first_block: u64,
    max_pcluster_bytes: usize,
) -> Result<Streamed> {
    let bs = out.bs;
    let size = content.size;
    let mut reader = (content.open)().map_err(io_error)?;
    let window = COMPRESS_WINDOW.max(2 * max_pcluster_bytes);
    let mut buf: Vec<u8> = Vec::with_capacity(window);
    // `buf[start..]` is what has been read and not yet put in a pcluster;
    // it begins at file offset `pos`.
    let mut start = 0usize;
    let mut pos = 0u64;
    let mut unread = size;
    let mut dst = vec![0u8; max_pcluster_bytes];
    let mut frame: Vec<u8> = Vec::with_capacity(max_pcluster_bytes);
    let mut block = first_block;
    // Each pcluster: where in the file it starts, and its plan.
    let mut pclusters: Vec<(u64, PclusterPlan)> = Vec::new();
    let mut addrs: Vec<u32> = Vec::new();
    let mut last: Vec<u8> = Vec::new();
    while pos < size {
        // Refill once less than half a window is left, moving what is
        // left to the front first: each byte moves at most once.
        if unread > 0 && buf.len() - start < window / 2 {
            buf.drain(..start);
            start = 0;
            let want = ((window - buf.len()) as u64).min(unread) as usize;
            let old = buf.len();
            buf.resize(old + want, 0);
            reader.read_exact(&mut buf[old..]).map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    Error::BadInode("file content is shorter than its size")
                } else {
                    io_error(e)
                }
            })?;
            unread -= want as u64;
        }
        let avail = &buf[start..];
        let (taken, written) = encoder.compress_dest_size(avail, &mut dst);
        let cblks = (written as u64).div_ceil(bs);
        addrs.push(block_addr(block)?);
        let (len, blocks, cluster_type) = if taken > 0 && cblks < (taken as u64).div_ceil(bs) {
            frame.clear();
            frame.resize((cblks * bs) as usize - written, 0);
            frame.extend_from_slice(&dst[..written]);
            out.write_blocks(block, &frame, cblks)?;
            (taken, cblks, Z_EROFS_LCLUSTER_TYPE_HEAD1)
        } else {
            // One block of the file, stored as it is. The kernel maps a
            // stored pcluster to one block of the file at most, from its
            // head up to the next head; with no head after it, that is to
            // the end of the lcluster the file ends in. So the last one,
            // if it starts inside a block and would run on into the next,
            // stops at the boundary and leaves the rest to a pcluster of
            // its own there.
            let into = (pos % bs) as usize;
            let len = if avail.len() > bs as usize || into == 0 {
                (bs as usize).min(avail.len())
            } else {
                avail.len().min(bs as usize - into)
            };
            out.write_blocks(block, &avail[..len], 1)?;
            if len < bs as usize {
                last = avail[..len].to_vec();
            }
            (len, 1, Z_EROFS_LCLUSTER_TYPE_PLAIN)
        };
        pclusters.push((
            pos,
            PclusterPlan {
                block_bytes: Vec::new(),
                cluster_type,
                pcluster_block_count: blocks as u32,
                raw_compressed: Vec::new(),
                head_lcluster_idx: (pos / bs) as u32,
                n_lclusters: 0,
            },
        ));
        pos += len as u64;
        start += len;
        block += blocks;
    }
    if reader.read(&mut [0u8; 1]).map_err(io_error)? != 0 {
        return Err(Error::BadInode("file content is longer than its size"));
    }
    if pclusters
        .iter()
        .all(|(_, p)| p.cluster_type == Z_EROFS_LCLUSTER_TYPE_PLAIN)
    {
        return Ok(Streamed::Stored { last });
    }

    // One index entry per block of the file. A pcluster's head is the
    // lcluster it starts in, recording where in it; every lcluster no
    // pcluster starts in belongs to the last one that started before it.
    // Each pcluster takes more than a block of the file but the last, so
    // no two start in one lcluster.
    let n_lclusters = size.div_ceil(bs) as usize;
    let mut heads: Vec<Option<LclusterIndexEntry>> = vec![None; n_lclusters];
    for (idx, (at, p)) in pclusters.iter().enumerate() {
        let lcn = (at / bs) as usize;
        if heads[lcn].is_some() {
            return Err(Error::BadInode("two pclusters start in one lcluster"));
        }
        heads[lcn] = Some(LclusterIndexEntry {
            cluster_type: p.cluster_type,
            clusterofs_or_delta0: (at % bs) as u16,
            pcluster_idx: idx as u32,
        });
    }
    let mut entries = Vec::with_capacity(n_lclusters);
    let mut owner = (0u32, 0usize);
    for (lcn, head) in heads.into_iter().enumerate() {
        match head {
            Some(entry) => {
                owner = (entry.pcluster_idx, lcn);
                entries.push(entry);
            }
            None => entries.push(LclusterIndexEntry {
                cluster_type: Z_EROFS_LCLUSTER_TYPE_NONHEAD,
                clusterofs_or_delta0: u16::try_from(lcn - owner.1)
                    .map_err(|_| Error::BadInode("pcluster spans more than 65535 lclusters"))?,
                pcluster_idx: owner.0,
            }),
        }
    }
    Ok(Streamed::Compressed {
        pclusters: pclusters.into_iter().map(|(_, p)| p).collect(),
        entries,
        addrs,
        blocks: block - first_block,
    })
}

/// The bytes left in a metadata block after an inode's body and xattrs:
/// the most an inline tail can hold, since the inode and its tail share
/// one block.
fn tail_room(body: &InodeBody, bs: u64) -> u64 {
    bs.saturating_sub(body.body_size + body.xattr_bytes.len() as u64)
}

/// A block number, as the 32 bits every on-disk block address has.
fn block_addr(block: u64) -> Result<u32> {
    u32::try_from(block).map_err(|_| Error::BadSuperblock("data block address > u32::MAX"))
}

/// The bytes a file's content is copied through on its way to the sink:
/// the only file data in memory at once.
const COPY_BUFFER: usize = 128 << 10;

/// The sink an image is written to, addressed in blocks.
struct ImageSink<'a, W: Write + Seek> {
    out: &'a mut W,
    bs: u64,
}

impl<'a, W: Write + Seek> ImageSink<'a, W> {
    fn new(out: &'a mut W, bs: u64) -> Self {
        Self { out, bs }
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.out
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(io_error)?;
        self.out.write_all(bytes).map_err(io_error)
    }

    /// Write `bytes` at block `blkaddr`, then zeros to the end of
    /// `blocks` blocks, so every block the image claims is written.
    fn write_blocks(&mut self, blkaddr: u64, bytes: &[u8], blocks: u64) -> Result<()> {
        let span = blocks * self.bs;
        if bytes.len() as u64 > span {
            return Err(Error::BadInode(
                "data longer than the blocks planned for it",
            ));
        }
        self.write_at(blkaddr * self.bs, bytes)?;
        self.write_zeros(span - bytes.len() as u64)
    }

    /// Zeros at the current position.
    fn write_zeros(&mut self, mut len: u64) -> Result<()> {
        const ZEROS: [u8; 4096] = [0; 4096];
        while len > 0 {
            let n = len.min(ZEROS.len() as u64) as usize;
            self.out.write_all(&ZEROS[..n]).map_err(io_error)?;
            len -= n as u64;
        }
        Ok(())
    }

    /// Copy a file's content to block `blkaddr`, through one bounded
    /// buffer, and pad its last block. Content that ends before its size,
    /// or goes on past it, is refused.
    ///
    /// The last `keep` bytes are returned rather than written, for an
    /// inline tail; with `keep` zero nothing is returned.
    fn write_content(&mut self, blkaddr: u64, content: Content, keep: u64) -> Result<Vec<u8>> {
        let size = content.size;
        let to_sink = size - keep;
        let mut kept = Vec::with_capacity(keep as usize);
        let mut reader = (content.open)().map_err(io_error)?;
        self.out
            .seek(std::io::SeekFrom::Start(blkaddr * self.bs))
            .map_err(io_error)?;
        let mut buf = vec![0u8; COPY_BUFFER.min(size as usize)];
        let mut left = size;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = match reader.read(&mut buf[..want]) {
                Ok(0) => return Err(Error::BadInode("file content is shorter than its size")),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(e)),
            };
            // Of these bytes, those before `to_sink` go to the sink.
            let done = size - left;
            let sink_part = (to_sink.saturating_sub(done) as usize).min(n);
            self.out.write_all(&buf[..sink_part]).map_err(io_error)?;
            kept.extend_from_slice(&buf[sink_part..n]);
            left -= n as u64;
        }
        if reader.read(&mut [0u8; 1]).map_err(io_error)? != 0 {
            return Err(Error::BadInode("file content is longer than its size"));
        }
        self.write_zeros(to_sink.next_multiple_of(self.bs) - to_sink)?;
        Ok(kept)
    }
}

// --- compression helpers (W2a) -----------------------------------------

/// One physical cluster's planning record: the bytes we'll write into
/// its block(s) and which lcluster type to record in the index.
///
/// Default policy: each lcluster owns exactly one pcluster (no
/// multi-lcluster collation supported by the greedy collator). With
/// `target_pcluster_blocks=1` (the default) `pcluster_block_count` is
/// always 1 OR 0 (ztailpacked LAST pcluster — no separate data block);
/// the field is recorded explicitly so future BIG_PCLUSTER work can
/// lift the constraint without restructuring callers.
///
/// A pcluster covers a contiguous range of source-byte input bytes:
/// `[lcluster_start * lcluster_size + clusterofs_at_head,
///   (lcluster_start + n_lclusters) * lcluster_size  OR  inode.size)`.
/// The pcluster's HEAD lcluster is at `lcluster_start`; subsequent
/// lclusters in `lcluster_start+1 .. lcluster_start+n_lclusters` are
/// recorded as NONHEAD entries (`delta[0] = lcluster_idx -
/// lcluster_start`).
#[derive(Debug)]
struct PclusterPlan {
    /// Bytes to write at `pcluster_blkaddr * block_size`, length is
    /// `pcluster_block_count * block_size`. For HEAD1 the LZ4 frame is
    /// right-aligned (leading zero pad) to satisfy
    /// `EROFS_FEATURE_INCOMPAT_ZERO_PADDING`. For PLAIN the source
    /// bytes are placed at offset 0 followed by trailing zero pad.
    /// Empty when `pcluster_block_count == 0` (the ztailpacked
    /// pcluster — its raw compressed bytes live in `raw_compressed`
    /// instead, to be inlined after the index area).
    block_bytes: Vec<u8>,
    /// `Z_EROFS_LCLUSTER_TYPE_HEAD1` (compressed LZ4) or
    /// `Z_EROFS_LCLUSTER_TYPE_PLAIN` (passthrough — engaged when the
    /// compressed payload is no smaller than the source). Indexed via
    /// `lcluster_entries` rather than read off PclusterPlan directly,
    /// hence `#[allow(dead_code)]`; kept on the struct for diagnostic
    /// dumps.
    #[allow(dead_code)]
    cluster_type: u8,
    /// On-disk block count of this pcluster. 1 normally; 0 for the
    /// ztailpacked LAST pcluster (which has no separate data block).
    pcluster_block_count: u32,
    /// Raw (un-padded) compressed-or-source bytes for this pcluster.
    /// Always populated; the legacy / non-inline path ignores this and
    /// uses `block_bytes` instead. The ztailpacking encoder reads
    /// this for the inline-tail emit path. For HEAD1 these are the
    /// LZ4 frame bytes; for PLAIN they are the raw source bytes
    /// (which is what the reader's PLAIN-passthrough dispatch
    /// expects when an inline-tail PLAIN pcluster is encountered).
    raw_compressed: Vec<u8>,
    /// Index of the first (HEAD) lcluster owned by this pcluster.
    /// Diagnostic / future-use only -- the encoder consults
    /// `lcluster_entries` (one per lcluster).
    #[allow(dead_code)]
    head_lcluster_idx: u32,
    /// Number of lclusters this pcluster spans (1 = no collation;
    /// \>= 2 = HEAD + (n-1) NONHEAD entries). Diagnostic / future-use
    /// only.
    #[allow(dead_code)]
    n_lclusters: u32,
}

/// One per-lcluster index entry the writer emits, normalised. Driven by
/// pass 5 (legacy + compacted-2B encoders). HEAD/PLAIN entries point at
/// their pcluster's `blkaddr`; NONHEAD entries record the back-distance
/// (in lclusters) to the owning HEAD via `delta0_or_clusterofs`.
///
/// Spec: `linux/fs/erofs/erofs_fs.h::z_erofs_lcluster_index` semantics
/// + per-pack bitstream layout described in the public EROFS
///   compression-format documentation
///   (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
#[derive(Debug, Clone, Copy)]
struct LclusterIndexEntry {
    /// `Z_EROFS_LCLUSTER_TYPE_*` (PLAIN/HEAD1/NONHEAD).
    cluster_type: u8,
    /// `clusterofs` for HEAD/PLAIN (always 0 in our writer — pcluster
    /// boundaries align with lcluster boundaries because we don't trim
    /// trailing source bytes off a pcluster). For NONHEAD this carries
    /// `delta[0]` (lclusters back to the owning HEAD).
    clusterofs_or_delta0: u16,
    /// Index into the parent `pclusters` vec identifying which pcluster
    /// owns this lcluster. The encoders use this to look up the
    /// pcluster's `blkaddr` (HEAD/PLAIN entries) -- NONHEAD entries
    /// don't carry a blkaddr in the on-disk layout.
    pcluster_idx: u32,
}

/// Compress one lcluster's source bytes via the requested codec.
/// Returns the compressed payload; the caller decides PLAIN-vs-HEAD1
/// by comparing length against `src.len()`.
///
/// Codec-specific output format:
/// - LZ4: raw block as `lz4_flex::block::compress` emits (no frame
///   header).
/// - LZMA: the 13-byte LZMA1 header (properties + unpacked_size) plus
///   the arithmetic-coded payload plus end marker, as
///   `lzma_rs::lzma_compress` emits with default options
///   (lc=3, lp=0, pb=2, dict_size = 1 << 24).
/// - DEFLATE: raw DEFLATE block (no zlib/gzip wrapper) using the
///   default compression level.
///
/// All three codecs are pure-Rust, dual MIT/Apache-2 licensed.
/// Independent implementation; not derived from any GPL'd EROFS
/// codebase (the kernel `decompressor_{lzma,deflate}.c` are GPL-2
/// and were not consulted).
/// Compress one lcluster's source bytes via the requested codec.
/// Optionally overrides the LZMA1 `dict_size` field in the on-disk
/// 13-byte header (lzma-rs hard-codes its own dict at 8 MiB; rewriting
/// the header bytes keeps the bitstream identical while advertising a
/// different `dict_size` in the COMPR_CFGS blob — which is what the
/// reader's `try_decompress_lzma_with_header` path will see when it
/// parses the in-stream header).
///
/// `lzma_dict_size_override` only affects [`CompressedAlgo::Lzma`]; the
/// other codecs ignore it. Passing `None` keeps the lzma-rs default.
fn compress_lcluster_with_cfg(
    algo: CompressedAlgo,
    src: &[u8],
    lzma_dict_size_override: Option<u32>,
) -> Vec<u8> {
    match algo {
        CompressedAlgo::Lz4 => lz4_flex::block::compress(src),
        CompressedAlgo::Lzma => {
            // lzma_rs writes to any `Write`; a Vec writer cannot fail.
            // Encode the actual unpacked size into the LZMA1 header
            // (instead of the default `0xFFFF_FFFF_FFFF_FFFF` "use end
            // marker" sentinel). With a known unpacked_size the decoder
            // stops after exactly that many output bytes, so the
            // trailing zero pad we write into the pcluster block is
            // silently ignored. Using the end-marker form would make
            // the decoder reject the trailing zeros with
            // "Found end-of-stream marker but more bytes are available".
            let mut out: Vec<u8> = Vec::new();
            let mut reader = std::io::Cursor::new(src);
            let opts = lzma_rs::compress::Options {
                unpacked_size: lzma_rs::compress::UnpackedSize::WriteToHeader(Some(
                    src.len() as u64
                )),
            };
            lzma_rs::lzma_compress_with_options(&mut reader, &mut out, &opts)
                .expect("lzma_compress on Vec writer cannot fail");
            // The LZMA1 header is 13 bytes: 1 byte properties, 4 bytes
            // dict_size (LE), 8 bytes unpacked_size (LE). Patch the
            // dict_size if the caller wants the on-disk advertised
            // value to match the COMPR_CFGS blob.
            if let Some(dict_size) = lzma_dict_size_override {
                if out.len() >= 13 {
                    out[1..5].copy_from_slice(&dict_size.to_le_bytes());
                }
            }
            out
        }
        CompressedAlgo::Deflate => {
            // `false` selects raw DEFLATE (no zlib header). flate2's
            // `write::DeflateEncoder` finishes cleanly on a Vec writer.
            use std::io::Write;
            let mut e =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(src)
                .expect("DeflateEncoder write_all on Vec writer cannot fail");
            e.finish()
                .expect("DeflateEncoder finish on Vec writer cannot fail")
        }
    }
}

/// Encode the 16-byte legacy `z_erofs_map_header` for a compressed
/// inode. The header is followed by an 8-byte reserved gap and then
/// the packed lcluster-index array; the gap is included in
/// `Z_EROFS_LEGACY_MAP_HEADER_SIZE = 16`.
///
/// Layout (little-endian):
/// ```text
/// 0x00..0x04 : __le32 fragment_off (or __le16 idata_size when ztailpacking)
/// 0x04..0x06 : __le16 advise   (0 = uncompacted/legacy default)
/// 0x06       : __u8   clusterbits_byte (low 4 bits = lclusterbits)
/// 0x07       : __u8   algorithm_type (0 = LZ4)
/// 0x08..0x10 : reserved (zero gap before the first lcluster_index)
/// ```
///
/// Spec: `linux/fs/erofs/erofs_fs.h::z_erofs_map_header`. Independent
/// implementation.
fn encode_zmap_header(advise: u16, lclusterbits: u8, algorithm: u8) -> [u8; 16] {
    let mut out = [0u8; 16];
    // fragment_off / idata_size: 0 (no ztailpacking, no fragments).
    out[0x00..0x04].copy_from_slice(&0u32.to_le_bytes());
    out[0x04..0x06].copy_from_slice(&advise.to_le_bytes());
    // Per the public spec: byte 6 = h_algorithmtype (low4 = HEAD1
    // algo, high4 = HEAD2 algo), byte 7 = h_clusterbits (low4 =
    // lclusterbits).
    out[0x06] = algorithm & 0x0F;
    out[0x07] = lclusterbits & 0x0F;
    // 0x08..0x10 left as zero (reserved gap; reader's
    // `Z_EROFS_LEGACY_MAP_HEADER_SIZE` accounts for this).
    out
}

/// Each lcluster's `delta[1]`: for a NONHEAD, the lclusters from it to
/// the next HEAD or PLAIN, or to one past the last lcluster when no head
/// follows it; zero for a head, which does not carry one.
fn lcluster_delta1(entries: &[LclusterIndexEntry]) -> Vec<u16> {
    let mut out = vec![0u16; entries.len()];
    let mut next_head = entries.len();
    for (i, e) in entries.iter().enumerate().rev() {
        if e.cluster_type == Z_EROFS_LCLUSTER_TYPE_NONHEAD {
            out[i] = u16::try_from(next_head - i).unwrap_or(u16::MAX);
        } else {
            next_head = i;
        }
    }
    out
}

/// Encode one 8-byte legacy `z_erofs_lcluster_index` entry.
///
/// Layout (little-endian):
/// ```text
/// 0x00..0x02 : __le16 di_advise   (bits 0..1 = cluster_type)
/// 0x02..0x04 : __le16 di_clusterofs
/// 0x04..0x08 : union { __le32 blkaddr; __le32 delta[2]; } u
/// ```
///
/// W2a only emits HEAD1 / PLAIN entries (no NONHEAD), so `u` is always
/// the pcluster blkaddr. Spec: `linux/fs/erofs/erofs_fs.h::z_erofs_lcluster_index`.
/// Independent implementation.
fn encode_lcluster_index(cluster_type: u8, clusterofs: u16, blkaddr_or_delta: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0x00..0x02].copy_from_slice(&(cluster_type as u16 & 0x3).to_le_bytes());
    out[0x02..0x04].copy_from_slice(&clusterofs.to_le_bytes());
    out[0x04..0x08].copy_from_slice(&blkaddr_or_delta.to_le_bytes());
    out
}

// --- W2b: compacted-2B index helpers -----------------------------------

/// Geometry of the compacted-2B mixed pack format (4B initial + 2B
/// middle + 4B trailing).
///
/// `compacted_4b_initial = ((32 - ebase % 32) / 4) & 7`, capped at
/// `totalidx`. When `Z_EROFS_ADVISE_COMPACTED_2B` is set on the header
/// AND `initial < totalidx`, the middle region is
/// `rounddown(totalidx - initial, 16)` lclusters in 2B form. The
/// remaining trailing lclusters return to 4B form.
///
/// Spec: `Z_EROFS_*` constants in the public EROFS format header
/// `erofs_fs.h`; pack geometry in the public EROFS compression-format
/// documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
/// Independent implementation.
#[derive(Debug, Clone, Copy)]
struct CompactGeom {
    initial: u32,
    middle: u32,
    tail: u32,
    /// Whether the middle (2B) region is actually emitted -- mirrors
    /// the `Z_EROFS_ADVISE_COMPACTED_2B` advise bit on the header.
    use_2b_middle: bool,
}

impl CompactGeom {
    /// Returns the on-disk byte length of the index area (no header,
    /// just the packs). Each region rounds UP to its pack boundary so a
    /// partial last pack still occupies a full pack's bytes (filled
    /// with zeros / sentinel entries).
    fn bytes(&self) -> u64 {
        let initial_bytes = ((self.initial as u64).div_ceil(2)) * 8;
        let middle_bytes = ((self.middle as u64).div_ceil(16)) * 32;
        let tail_bytes = ((self.tail as u64).div_ceil(2)) * 8;
        initial_bytes + middle_bytes + tail_bytes
    }

    fn totalidx(&self) -> u32 {
        self.initial + self.middle + self.tail
    }
}

/// Compute compacted-2B pack geometry given `ebase` (on-disk byte
/// offset of the first pack), the lcluster count, and whether the
/// 2B middle region should be enabled.
///
/// `ebase = ALIGN(body_end, 8) + sizeof(map_header)` — same as the
/// reader's `ZMap::open`. The initial 4B region's lcluster count is
/// `((32 - ebase % 32) / 4) & 7`, which makes the 2B middle region
/// (starting `initial_bytes = ceil(initial/2) * 8` bytes later) land
/// on a 32-byte boundary. fsck.erofs and the reader both rely on this
/// alignment.
///
/// Spec: 32-byte pack alignment described in the public EROFS
/// compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
/// Independent implementation.
fn compute_compact_geom(ebase: u64, totalidx: u32, want_2b_middle: bool) -> CompactGeom {
    if totalidx == 0 {
        return CompactGeom {
            initial: 0,
            middle: 0,
            tail: 0,
            use_2b_middle: false,
        };
    }
    let pad = (((32 - (ebase % 32)) / 4) & 7) as u32;
    let initial = pad.min(totalidx);
    let (middle, tail, use_2b_middle) = if want_2b_middle && initial < totalidx {
        let remaining = totalidx - initial;
        let middle = remaining - (remaining % 16);
        let tail = remaining - middle;
        (middle, tail, middle > 0)
    } else {
        (0, totalidx - initial, false)
    };
    CompactGeom {
        initial,
        middle,
        tail,
        use_2b_middle,
    }
}

/// Encode a single (cluster_type, lo) pair into the bitstream of a
/// compact pack. `bit_pos` is the start bit position within the
/// bitstream byte slice; `lobits` is the per-entry `lo` field width
/// (max(z_lclusterbits, 12)).
///
/// The encoded value is `(cluster_type << lobits) | lo`, then OR-merged
/// into the destination starting at `bit_pos / 8`. Adjacent entries can
/// share the same byte boundary (4B form: 16-bit-aligned, no overlap;
/// 2B form: 14-bit entries straddle byte boundaries).
///
/// Spec: bitstream layout described in the public EROFS
/// compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>);
/// the encoder is the obvious inverse of the documented decoder.
/// Independent implementation.
fn pack_write_entry(bitstream: &mut [u8], bit_pos: usize, lobits: u32, cluster_type: u8, lo: u32) {
    debug_assert!(lobits < 32);
    let lo_mask = (1u32 << lobits) - 1;
    let value = ((cluster_type as u32 & 0x3) << lobits) | (lo & lo_mask);
    let shifted = (value as u64) << (bit_pos % 8);
    let byte = bit_pos / 8;
    for k in 0..8 {
        if byte + k < bitstream.len() {
            bitstream[byte + k] |= ((shifted >> (k * 8)) & 0xFF) as u8;
        }
    }
}

/// One per-lcluster index entry, normalised. Compacted-2B emits these
/// in the order they appear in the file; the encoder slices them into
/// packs and computes per-pack base blkaddrs from them.
#[derive(Debug, Clone, Copy)]
struct CompactEntry {
    cluster_type: u8,
    lo: u32,
    /// Absolute pcluster blkaddr for HEAD/PLAIN entries; 0 for the
    /// ztailpacked LAST lcluster (whose pcluster_block_count == 0 so
    /// no real blkaddr exists). NONHEAD entries are not emitted by
    /// our writer (one pcluster per lcluster, no collation).
    pcluster_blkaddr: u32,
}

/// Encode the compacted-2B index area for a compressed inode.
///
/// Pre-conditions:
/// - `entries[i]` describes the `i`-th lcluster in file order.
/// - HEAD/PLAIN entries carry their resolved pcluster blkaddr in
///   `pcluster_blkaddr`; NONHEAD entries carry `delta[0]` in `lo` and
///   their `pcluster_blkaddr` is unused.
/// - `geom` has been computed with [`compute_compact_geom`] using the
///   same `ebase` the reader will derive.
///
/// Each pack's per-pack base blkaddr (the trailing `__le32`) is set so
/// the reader's `pblk = base + nblk` arithmetic resolves the absolute
/// pcluster blkaddr of every HEAD/PLAIN entry in the pack. The reader
/// computes `nblk` for the FIRST HEAD/PLAIN entry of a pack as `1`
/// (preceding entries can only be NONHEADs whose owning HEAD is in an
/// earlier pack -- the back-walk steps off the start of the pack and
/// stops). Solving: `base = first_head_blkaddr - 1`, regardless of
/// where that first HEAD/PLAIN sits within the pack.
///
/// All-NONHEAD packs (every entry's owning HEAD lives in an earlier
/// pack) don't carry a meaningful base because no entry inside the
/// pack reads it; we still emit a deterministic 0 for fsck/repro
/// stability.
///
/// Unused trailing slots of a partial last pack are left as
/// `(type=PLAIN, lo=0)` — the reader never indexes past `n_lclusters`
/// so the slot value is irrelevant; we just need the bitstream bytes
/// to be deterministic / fsck-clean (zeros).
///
/// Spec: pack geometry + bitstream encoding described in the public
/// EROFS compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
/// Independent implementation.
fn encode_compact2b_index(geom: &CompactGeom, entries: &[CompactEntry], lobits: u32) -> Vec<u8> {
    debug_assert_eq!(entries.len() as u32, geom.totalidx());
    let total_bytes = geom.bytes() as usize;
    let mut out = vec![0u8; total_bytes];

    // Walk packs in order, slicing `entries` accordingly.
    let mut entry_cursor = 0usize;
    let mut byte_cursor = 0usize;

    // One pack of `shape`, holding `entries_slice`.
    //
    // The shape used to arrive as three unlabelled integers — and the
    // third, `vcnt`, was discarded on the next line but one. It looked
    // like it governed how many entries the pack takes; the caller's
    // `take` does that, and `entries_slice.len()` is the real count.
    // A `let _ = vcnt;` is not a use.
    let mut emit_pack = |entries_slice: &[CompactEntry], shape: CompactPackShape| {
        let pack_bytes = shape.pack_bytes as usize;
        let encodebits = shape.encodebits as usize;
        let bitstream_len = pack_bytes - 4;
        // base = (first HEAD/PLAIN entry's blkaddr) - 1 (its nblk
        // is 1). All-NONHEAD packs use 0 as a deterministic stub.
        let base = entries_slice
            .iter()
            .find(|e| e.cluster_type != Z_EROFS_LCLUSTER_TYPE_NONHEAD)
            .map(|e| e.pcluster_blkaddr.saturating_sub(1))
            .unwrap_or(0);
        let pack_dst = &mut out[byte_cursor..byte_cursor + pack_bytes];
        // Bitstream first.
        let bitstream = &mut pack_dst[..bitstream_len];
        for (i, e) in entries_slice.iter().enumerate() {
            pack_write_entry(bitstream, i * encodebits, lobits, e.cluster_type, e.lo);
        }
        // Trailing slots inside the same pack: leave as zeros (the
        // mask above already zero-initialised the buffer). The
        // reader never reads past totalidx so these don't matter.
        // Trailing __le32 base blkaddr.
        pack_dst[bitstream_len..bitstream_len + 4].copy_from_slice(&base.to_le_bytes());
        byte_cursor += pack_bytes;
    };

    // Initial 4B region.
    let mut remaining_initial = geom.initial as usize;
    while remaining_initial > 0 {
        let take = remaining_initial.min(COMPACT_4B.vcnt as usize);
        emit_pack(&entries[entry_cursor..entry_cursor + take], COMPACT_4B);
        entry_cursor += take;
        remaining_initial -= take;
    }
    // Middle 2B region (if enabled).
    if geom.use_2b_middle {
        let mut remaining_middle = geom.middle as usize;
        while remaining_middle > 0 {
            let take = remaining_middle.min(COMPACT_2B.vcnt as usize);
            emit_pack(&entries[entry_cursor..entry_cursor + take], COMPACT_2B);
            entry_cursor += take;
            remaining_middle -= take;
        }
    }
    // Trailing 4B region.
    let mut remaining_tail = geom.tail as usize;
    while remaining_tail > 0 {
        let take = remaining_tail.min(COMPACT_4B.vcnt as usize);
        emit_pack(&entries[entry_cursor..entry_cursor + take], COMPACT_4B);
        entry_cursor += take;
        remaining_tail -= take;
    }

    debug_assert_eq!(entry_cursor, entries.len());
    debug_assert_eq!(byte_cursor, total_bytes);
    out
}

/// Plan the per-pcluster compressed payload for a `CompressedFile`.
///
/// Greedy multi-lcluster pcluster collation (Option A): for each
/// lcluster, try APPENDING its source bytes to the current pcluster's
/// trial buffer and recompressing. If the combined frame still fits in
/// `target_pcluster_blocks * block_size` bytes (and is shorter than the
/// raw source — otherwise the pcluster opts out and emits PLAIN), the
/// lcluster is absorbed as a NONHEAD entry pointing back to the head.
/// Otherwise the current pcluster is committed and a fresh HEAD pcluster
/// starts with this lcluster's bytes alone.
///
/// PLAIN fallback stays per-lcluster: a single uncompressible lcluster
/// closes the open pcluster and emits as its own PLAIN pcluster.
///
/// Returns `(pclusters, lcluster_entries, total_block_count)`. With
/// `target_pcluster_blocks=1` (the spec default and only currently
/// supported value because we don't ship BIG_PCLUSTER plumbing), every
/// pcluster occupies exactly one on-disk block. The collator's value-
/// add is grouping multiple lclusters into ONE pcluster when their
/// combined LZ4 frame fits in 1 block — letting the LZ4 dictionary
/// span the whole group instead of restarting at every lcluster
/// boundary.
///
/// Spec: pcluster source-byte-range derivation
/// (`[head*lcsize+head.clusterofs, next_head*lcsize+next.clusterofs)`)
/// described in the public EROFS compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
/// Independent implementation; the kernel's `mkfs.erofs` collator uses
/// a similar "compress N lclusters, see if it fits" strategy but its
/// concrete block-budget arithmetic and PLAIN-fallback heuristics
/// differ.
#[cfg(test)]
#[allow(clippy::type_complexity)]
fn plan_compressed_pclusters(
    src: &[u8],
    algo: CompressedAlgo,
    lclusterbits: u8,
    bs: u64,
    target_pcluster_blocks: u32,
) -> Result<(Vec<PclusterPlan>, Vec<LclusterIndexEntry>, u32)> {
    plan_compressed_pclusters_with_cfg(src, algo, lclusterbits, bs, target_pcluster_blocks, None)
}

/// As [`plan_compressed_pclusters`] but threads an LZMA `dict_size`
/// override into [`compress_lcluster_with_cfg`]. `None` is identical
/// to calling [`plan_compressed_pclusters`].
#[allow(clippy::type_complexity)]
fn plan_compressed_pclusters_with_cfg(
    src: &[u8],
    algo: CompressedAlgo,
    lclusterbits: u8,
    bs: u64,
    target_pcluster_blocks: u32,
    lzma_dict_size_override: Option<u32>,
) -> Result<(Vec<PclusterPlan>, Vec<LclusterIndexEntry>, u32)> {
    if lclusterbits > 4 {
        // The lclusterbits field lives in the low 4 bits of i_format's
        // flag nibble, so values > 15 are unrepresentable; even values
        // 5..=15 push lcluster_size past 2 MiB, which W2a doesn't aim
        // to exercise. Reject early so the caller gets a clean error
        // instead of a silently truncated header.
        return Err(Error::BadInode("lclusterbits > 4 not supported in W2a"));
    }
    let lcluster_size = bs << lclusterbits;
    if lcluster_size == 0 {
        return Err(Error::BadInode("lcluster_size overflow"));
    }
    let n_lclusters = if src.is_empty() {
        0
    } else {
        (src.len() as u64).div_ceil(lcluster_size)
    };
    // Treat 0 as 1: the planner always needs a non-zero block budget.
    let target_blocks = target_pcluster_blocks.max(1);
    let pcluster_budget_bytes: usize = (bs as usize).saturating_mul(target_blocks as usize);

    let mut pclusters: Vec<PclusterPlan> = Vec::new();
    let mut lcluster_entries: Vec<LclusterIndexEntry> = Vec::with_capacity(n_lclusters as usize);
    let mut total_blocks: u32 = 0;

    // Open-pcluster state. `None` when there's no pcluster currently
    // accumulating lclusters.
    let mut open: Option<OpenPcluster> = None;

    for i in 0..n_lclusters {
        let start = (i * lcluster_size) as usize;
        let end = (start + lcluster_size as usize).min(src.len());
        let chunk = &src[start..end];

        // Try to extend the open pcluster with this lcluster.
        let extended = if let Some(ref cur) = open {
            if cur.cluster_type != Z_EROFS_LCLUSTER_TYPE_HEAD1 {
                None
            } else {
                let mut trial_src = cur.src_buf.clone();
                trial_src.extend_from_slice(chunk);
                let trial_compressed =
                    compress_lcluster_with_cfg(algo, &trial_src, lzma_dict_size_override);
                if trial_compressed.len() <= pcluster_budget_bytes
                    && trial_compressed.len() < trial_src.len()
                {
                    Some((trial_src, trial_compressed))
                } else {
                    None
                }
            }
        } else {
            None
        };

        if let Some((new_src, new_compressed)) = extended {
            let cur = open.as_mut().unwrap();
            let pcluster_idx = pclusters.len() as u32;
            // delta[0] = lcluster_idx - head_lcluster_idx. Bounded by
            // 0xFFFF in the legacy on-disk encoding (and 14 bits on
            // the 2B compact path). With lcluster_size >= 512 B and a
            // 1-block pcluster budget, a real LZ4 frame can't credibly
            // own that many lclusters, but we still refuse to overflow.
            let delta0 = (i as u32) - cur.head_lcluster_idx;
            if delta0 > u16::MAX as u32 {
                return Err(Error::BadInode(
                    "pcluster spans more lclusters than NONHEAD delta[0] can encode",
                ));
            }
            cur.src_buf = new_src;
            cur.compressed = new_compressed;
            cur.n_lclusters += 1;
            lcluster_entries.push(LclusterIndexEntry {
                cluster_type: Z_EROFS_LCLUSTER_TYPE_NONHEAD,
                clusterofs_or_delta0: delta0 as u16,
                pcluster_idx,
            });
            continue;
        }

        // Close any open pcluster before opening a fresh one.
        if let Some(prev) = open.take() {
            let pc = prev.finalize(bs, target_blocks, algo)?;
            total_blocks = total_blocks
                .checked_add(pc.pcluster_block_count)
                .ok_or(Error::BadInode("compressed blocks overflow u32"))?;
            pclusters.push(pc);
        }

        // Start a fresh pcluster. Try HEAD1 first; fall back to PLAIN
        // if the LZ4 frame doesn't shrink the source or won't fit.
        let single_compressed = compress_lcluster_with_cfg(algo, chunk, lzma_dict_size_override);
        let cluster_type = if single_compressed.len() < chunk.len()
            && single_compressed.len() <= pcluster_budget_bytes
        {
            Z_EROFS_LCLUSTER_TYPE_HEAD1
        } else {
            Z_EROFS_LCLUSTER_TYPE_PLAIN
        };
        let pcluster_idx = pclusters.len() as u32;
        lcluster_entries.push(LclusterIndexEntry {
            cluster_type,
            clusterofs_or_delta0: 0,
            pcluster_idx,
        });

        if cluster_type == Z_EROFS_LCLUSTER_TYPE_PLAIN {
            // PLAIN pclusters never collate. Commit immediately.
            let pc = PclusterPlan {
                block_bytes: pad_pcluster_block(
                    chunk,
                    bs,
                    target_blocks,
                    Z_EROFS_LCLUSTER_TYPE_PLAIN,
                    algo,
                )?,
                cluster_type,
                pcluster_block_count: target_blocks,
                raw_compressed: chunk.to_vec(),
                head_lcluster_idx: i as u32,
                n_lclusters: 1,
            };
            total_blocks = total_blocks
                .checked_add(target_blocks)
                .ok_or(Error::BadInode("compressed blocks overflow u32"))?;
            pclusters.push(pc);
        } else {
            // HEAD1: keep the pcluster open so the next lcluster can
            // try to absorb in.
            open = Some(OpenPcluster {
                head_lcluster_idx: i as u32,
                n_lclusters: 1,
                src_buf: chunk.to_vec(),
                compressed: single_compressed,
                cluster_type: Z_EROFS_LCLUSTER_TYPE_HEAD1,
            });
        }
    }

    if let Some(prev) = open.take() {
        let pc = prev.finalize(bs, target_blocks, algo)?;
        total_blocks = total_blocks
            .checked_add(pc.pcluster_block_count)
            .ok_or(Error::BadInode("compressed blocks overflow u32"))?;
        pclusters.push(pc);
    }

    Ok((pclusters, lcluster_entries, total_blocks))
}

/// In-progress pcluster the greedy collator is accumulating into.
struct OpenPcluster {
    head_lcluster_idx: u32,
    n_lclusters: u32,
    /// Concatenated source bytes for the whole pcluster so far.
    src_buf: Vec<u8>,
    /// Last successful compressed frame for `src_buf`.
    compressed: Vec<u8>,
    /// Pcluster type. Only HEAD1 collates; PLAIN closes immediately.
    cluster_type: u8,
}

impl OpenPcluster {
    fn finalize(self, bs: u64, target_blocks: u32, algo: CompressedAlgo) -> Result<PclusterPlan> {
        let block_bytes =
            pad_pcluster_block(&self.compressed, bs, target_blocks, self.cluster_type, algo)?;
        Ok(PclusterPlan {
            block_bytes,
            cluster_type: self.cluster_type,
            pcluster_block_count: target_blocks,
            raw_compressed: self.compressed,
            head_lcluster_idx: self.head_lcluster_idx,
            n_lclusters: self.n_lclusters,
        })
    }
}

/// Right-align (LZ4) or zero-pad-trailing (LZMA / DEFLATE) the
/// compressed payload into a `target_blocks * bs`-byte buffer. PLAIN
/// payloads (raw source bytes) always land at offset 0 with trailing
/// zero pad, regardless of codec.
///
/// Spec: `EROFS_FEATURE_INCOMPAT_ZERO_PADDING` (LZ4 leading zero pad).
/// Independent implementation.
fn pad_pcluster_block(
    payload: &[u8],
    bs: u64,
    target_blocks: u32,
    cluster_type: u8,
    algo: CompressedAlgo,
) -> Result<Vec<u8>> {
    let total: usize = (bs as usize)
        .checked_mul(target_blocks as usize)
        .ok_or(Error::BadInode("pcluster total bytes overflow"))?;
    if payload.len() > total {
        return Err(Error::BadInode("pcluster payload exceeds block budget"));
    }
    let mut block = vec![0u8; total];
    if cluster_type == Z_EROFS_LCLUSTER_TYPE_HEAD1 {
        match algo {
            CompressedAlgo::Lz4 => {
                let pad = total - payload.len();
                block[pad..].copy_from_slice(payload);
            }
            CompressedAlgo::Lzma | CompressedAlgo::Deflate => {
                block[..payload.len()].copy_from_slice(payload);
            }
        }
    } else {
        // PLAIN
        block[..payload.len()].copy_from_slice(payload);
    }
    Ok(block)
}

// --- planner internals -------------------------------------------------

/// Internal flat representation of a node post-flatten. `parent_idx` is
/// the index in the plan list, NOT a NID -- those aren't assigned until
/// the second pass. We translate idx->NID via the `nids` map.
#[derive(Debug)]
struct PlanNode {
    parent_idx: u64,
    mode: u16,
    meta: NodeMeta,
    xattrs: Vec<XattrSpec>,
    kind: PlanKind,
}

#[derive(Debug)]
enum PlanKind {
    File {
        /// Taken when the bytes are read: into the inline tail while
        /// planning, or straight into the data area.
        content: Option<Content>,
        size: u64,
        layout: FileLayout,
    },
    Dir {
        children: Vec<(String, u64)>, // (name, child_idx)
    },
    Symlink {
        target: Vec<u8>,
    },
    Device {
        rdev: u32,
    },
    Special,
    Chunked {
        chunks: Vec<Option<Vec<u8>>>,
        chunk_bits: u8,
        use_indexed_format: bool,
    },
    Compressed {
        /// Original (uncompressed) source bytes, for a
        /// [`Node::CompressedFile`]; released once `pclusters` are
        /// planned from them. Empty for a file compressed as it streams.
        src: Vec<u8>,
        /// The uncompressed length: the inode's `i_size`.
        size: u64,
        /// Pclusters may span more than one block, so the index carries
        /// each one's block count (`Z_EROFS_ADVISE_BIG_PCLUSTER_1`).
        big_pcluster: bool,
        algo: CompressedAlgo,
        lclusterbits: u8,
        /// One entry per *physical* cluster. With multi-lcluster
        /// collation enabled (`target_pcluster_blocks > 0` and at
        /// least two lclusters whose combined LZ4 frame fits within the
        /// pcluster budget), one `PclusterPlan` can cover multiple
        /// logical clusters; the per-lcluster index records that via
        /// NONHEAD entries pointing back to the head.
        ///
        /// When `ztailpacking_inline` is `Some`, the LAST pcluster has
        /// `pcluster_block_count == 0` and its bytes are instead
        /// exposed via `ztailpacking_inline` (written in the metadata
        /// area, not in a data block).
        pclusters: Vec<PclusterPlan>,
        /// One entry per *logical* cluster. Drives both the legacy
        /// 8-byte-per-lcluster encoder and the compacted-2B
        /// bitstream encoder. HEAD/PLAIN entries reference their
        /// pcluster's blkaddr via `pcluster_idx`; NONHEAD entries
        /// carry `delta[0]` in `clusterofs_or_delta0`.
        lcluster_entries: Vec<LclusterIndexEntry>,
        /// Total on-disk blocks consumed by `pclusters` (== sum of
        /// `pcluster_block_count`). Stored in `i_u`.
        total_compressed_blocks: u32,
        /// Index encoding: 8-byte-per-lcluster legacy or compacted-2B
        /// pack-based.
        index_format: CompressedIndexFormat,
        /// True when the spec asked for ztailpacking. The body planner
        /// decides whether the inline-tail bytes actually fit; if they
        /// do, `ztailpacking_inline` is set and the corresponding
        /// `pclusters[last].pcluster_block_count` is zeroed.
        ztailpacking_requested: bool,
        /// When `Some`, the LAST pcluster's compressed bytes are
        /// inlined immediately after the index area (ztailpacking).
        ztailpacking_inline: Option<ZtailInline>,
        /// Greedy collator budget in blocks (see
        /// [`CompressedFileSpec::target_pcluster_blocks`]). 0 here is
        /// treated as 1 to keep planner / encoder math simple.
        target_pcluster_blocks: u32,
    },
}

/// One inode's ztailpacking inline-tail bytes, kept alongside the
/// pcluster plan so pass 5 can splat them into the metadata area.
#[derive(Debug)]
struct ZtailInline {
    /// Compressed (HEAD1) or raw-source (PLAIN) payload bytes to
    /// inline directly after the index area. Length must fit in the
    /// `h_idata_size` u16 (the spec slot in the header union).
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileLayout {
    FlatPlain,
    FlatInline,
}

/// Pre-computed metadata-area sizes for one inode.
struct InodeBody {
    body_size: u64, // 32 or 64
    is_extended: bool,
    xattr_bytes: Vec<u8>,
    xattr_icount: u16,
    inline_tail: Vec<u8>,
    chunkmap_bytes: u64,
    /// Combined size of the legacy zmap header (16) + lcluster index
    /// array (8 * n_lclusters). Zero for non-compressed inodes.
    zmap_bytes: u64,
}

/// Where one inode sits in the metadata area.
///
/// `start` is a byte offset relative to the start of the metadata area
/// (`meta_blkaddr * bs`), and is always a multiple of
/// [`COMPACT_INODE_SIZE`] — so `start / COMPACT_INODE_SIZE` is exactly
/// the inode's NID, and `nid * COMPACT_INODE_SIZE` is exactly the
/// address its bytes go to.
///
/// That round trip is the point of this type. The NID recorded in
/// directory entries and the address the inode bytes are written to are
/// two readings of one number, so they cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MetaSlot {
    start: u64,
    /// Body + inline xattrs + inline tail + chunkmap + zmap trailer.
    span: u64,
}

impl MetaSlot {
    /// The NID that names this inode. `start` is slot-aligned by
    /// construction, so this division is exact.
    fn nid(&self) -> u64 {
        self.start / COMPACT_INODE_SIZE
    }

    /// One past the last byte this inode owns.
    fn end(&self) -> u64 {
        self.start + self.span
    }
}

/// Bytes an inode occupies in the metadata area: the fixed-size body
/// plus every trailer that must sit immediately behind it.
///
/// [`plan_meta_layout`] calls this to place the inode; the emission loop
/// in [`build_image_with`] calls it (via [`MetaSlot::end`]) to confirm
/// the bytes it wrote filled exactly the span that was planned. A new
/// trailer added to one and not the other is caught rather than
/// silently overwriting the next inode.
fn body_span(body: &InodeBody) -> u64 {
    body.body_size
        + body.xattr_bytes.len() as u64
        + body.inline_tail.len() as u64
        + body.chunkmap_bytes
        + body.zmap_bytes
}

/// Walk the metadata area once and hand back where every inode goes.
///
/// Two placement rules, applied per inode:
///
/// 1. An inode starts on a [`COMPACT_INODE_SIZE`]-byte slot boundary,
///    because a NID *is* a slot index.
/// 2. Spec invariant: an inode body together with its inline xattrs,
///    FLAT_INLINE tail and chunkmap MUST live in a single block. If the
///    combined size will not fit in the remainder of the current
///    metadata block, skip to the next block so the inode re-anchors at
///    a fresh block boundary. (FLAT_PLAIN inodes without inline tails
///    tolerate crossings, but it is simplest to enforce the same rule
///    for everything.)
///
/// This function is the only place those rules exist. Both the NIDs
/// written into directory entries and the addresses the inode bytes are
/// written to are read out of the slots it returns, so there is no
/// second copy to drift away from this one.
///
/// `bs` is a power of two no smaller than 512 (blkszbits is validated to
/// `9..=16` on entry to [`build_image_with`]), hence a multiple of
/// [`COMPACT_INODE_SIZE`] — so rule 2 can never land a start off a slot
/// boundary.
fn plan_meta_layout(bodies: &[InodeBody], bs: u64) -> Vec<MetaSlot> {
    let mut slots = Vec::with_capacity(bodies.len());
    let mut cursor: u64 = 0;
    for body in bodies {
        cursor = cursor.div_ceil(COMPACT_INODE_SIZE) * COMPACT_INODE_SIZE;
        let span = body_span(body);
        let trailers = span - body.body_size;
        if trailers > 0 && cursor % bs + span > bs {
            cursor = cursor.div_ceil(bs) * bs;
        }
        slots.push(MetaSlot {
            start: cursor,
            span,
        });
        cursor += span;
    }
    slots
}

/// A [`Node::HardLink`] met while flattening: the directory it sits in,
/// its name there, and the path it names. Resolved once the whole tree is
/// in the plan, since the target may come later in the walk.
struct PendingLink {
    dir_idx: u64,
    name: String,
    target: String,
}

fn flatten(
    node: Node,
    parent_idx: u64,
    plan: &mut Vec<PlanNode>,
    links: &mut Vec<PendingLink>,
) -> Result<u64> {
    let idx = plan.len() as u64;
    plan.push(PlanNode {
        parent_idx,
        mode: 0,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
        kind: PlanKind::Special,
    });
    match node {
        Node::File {
            mode,
            data,
            meta,
            xattrs,
        } => {
            if (mode & 0xF000) != S_IFREG {
                return Err(Error::BadInode("File mode missing S_IFREG"));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::File {
                size: data.len() as u64,
                content: Some(Content::from_bytes(data)),
                layout: FileLayout::FlatPlain,
            };
        }
        Node::Dir {
            mode,
            entries,
            meta,
            xattrs,
        } => {
            if (mode & 0xF000) != S_IFDIR {
                return Err(Error::BadInode("Dir mode missing S_IFDIR"));
            }
            let mut children = Vec::with_capacity(entries.len());
            for (name, child) in entries {
                if name.is_empty() || name.contains('/') || name.as_bytes().contains(&0) {
                    return Err(Error::BadDirent("invalid name"));
                }
                if name.len() > u16::MAX as usize {
                    return Err(Error::BadDirent("name length > 65535"));
                }
                if let Node::HardLink { target } = child {
                    links.push(PendingLink {
                        dir_idx: idx,
                        name,
                        target,
                    });
                    continue;
                }
                let child_idx = flatten(child, idx, plan, links)?;
                children.push((name, child_idx));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::Dir { children };
        }
        Node::Symlink {
            mode,
            target,
            meta,
            xattrs,
        } => {
            if (mode & 0xF000) != S_IFLNK {
                return Err(Error::BadInode("Symlink mode missing S_IFLNK"));
            }
            if target.is_empty() {
                return Err(Error::BadInode("symlink target empty"));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::Symlink {
                target: target.into_bytes(),
            };
        }
        Node::Device {
            mode,
            rdev,
            meta,
            xattrs,
        } => {
            let mt = mode & 0xF000;
            if mt != S_IFCHR && mt != S_IFBLK {
                return Err(Error::BadInode("Device mode must be S_IFCHR or S_IFBLK"));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::Device { rdev };
        }
        Node::Special { mode, meta, xattrs } => {
            let mt = mode & 0xF000;
            if mt != S_IFIFO && mt != S_IFSOCK {
                return Err(Error::BadInode("Special mode must be S_IFIFO or S_IFSOCK"));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::Special;
        }
        Node::ChunkedFile {
            mode,
            chunk_bits,
            chunks,
            use_indexed_format,
            meta,
            xattrs,
        } => {
            if (mode & 0xF000) != S_IFREG {
                return Err(Error::BadInode("ChunkedFile mode missing S_IFREG"));
            }
            if chunk_bits as u16 > EROFS_CHUNK_FORMAT_BLKBITS_MASK {
                return Err(Error::BadInode("chunk_bits > 31"));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::Chunked {
                chunks,
                chunk_bits,
                use_indexed_format,
            };
        }
        Node::CompressedFile(spec) => {
            if (spec.mode & 0xF000) != S_IFREG {
                return Err(Error::BadInode("CompressedFile mode missing S_IFREG"));
            }
            // Compression planning happens here at flatten time so the
            // body planner can size the zmap-trailer up front. The
            // actual block-byte placement (which depends on `bs`) is
            // re-derived in pass 4 from `lclusterbits` + `src`.
            plan[idx as usize].mode = spec.mode;
            plan[idx as usize].meta = spec.meta;
            plan[idx as usize].xattrs = spec.xattrs;
            plan[idx as usize].kind = PlanKind::Compressed {
                size: spec.data.len() as u64,
                big_pcluster: false,
                src: spec.data,
                algo: spec.algo,
                lclusterbits: spec.lclusterbits,
                // Filled in during plan_body once we know `bs`.
                pclusters: Vec::new(),
                lcluster_entries: Vec::new(),
                total_compressed_blocks: 0,
                index_format: spec.index_format,
                ztailpacking_requested: spec.ztailpacking,
                ztailpacking_inline: None,
                target_pcluster_blocks: spec.target_pcluster_blocks,
            };
        }
        Node::HardLink { .. } => return Err(Error::BadInode("the root is a hard link")),
        Node::StreamedFile {
            mode,
            content,
            meta,
            xattrs,
        } => {
            if (mode & 0xF000) != S_IFREG {
                return Err(Error::BadInode("StreamedFile mode missing S_IFREG"));
            }
            plan[idx as usize].mode = mode;
            plan[idx as usize].meta = meta;
            plan[idx as usize].xattrs = xattrs;
            plan[idx as usize].kind = PlanKind::File {
                size: content.size(),
                content: Some(content),
                layout: FileLayout::FlatPlain,
            };
        }
    }
    Ok(idx)
}

/// Give every [`PendingLink`] its directory entry, naming the plan node
/// its target path reaches from the root. Links are not in any
/// directory's children yet, so a link to a link finds nothing and is
/// refused, as is a link to a directory, which would give the tree a
/// cycle and the directory a second parent.
fn resolve_hard_links(plan: &mut [PlanNode], links: Vec<PendingLink>) -> Result<()> {
    let mut resolved = Vec::with_capacity(links.len());
    for link in &links {
        let mut idx = 0usize;
        for component in link.target.split('/').filter(|c| !c.is_empty()) {
            let PlanKind::Dir { children } = &plan[idx].kind else {
                return Err(Error::BadInode(
                    "hard link target runs through a non-directory",
                ));
            };
            // `children` is in `BTreeMap` order, which is byte order.
            let at = children
                .binary_search_by(|(name, _)| name.as_bytes().cmp(component.as_bytes()))
                .map_err(|_| Error::BadInode("hard link target not found"))?;
            idx = children[at].1 as usize;
        }
        if let PlanKind::Dir { .. } = plan[idx].kind {
            return Err(Error::BadInode("hard link target is a directory"));
        }
        resolved.push(idx as u64);
    }
    for (link, target_idx) in links.into_iter().zip(resolved) {
        if let PlanKind::Dir { children } = &mut plan[link.dir_idx as usize].kind {
            children.push((link.name, target_idx));
        }
    }
    Ok(())
}

/// Every inode's link count, by plan index. A directory has one for
/// itself, one for its "." and one for each subdirectory's ".."; anything
/// else has one per directory entry that names it, and at least one, for
/// a root that is not a directory.
fn link_counts(plan: &[PlanNode]) -> Vec<u32> {
    let mut nlinks = vec![0u32; plan.len()];
    for node in plan {
        if let PlanKind::Dir { children } = &node.kind {
            for (_, child_idx) in children {
                let child = &mut nlinks[*child_idx as usize];
                *child = child.saturating_add(1);
            }
        }
    }
    for (count, node) in nlinks.iter_mut().zip(plan) {
        *count = match &node.kind {
            // Spec: a directory's nlink = 2 (for "." and "..") + the
            // number of subdirectories (each subdir's ".." link counts
            // toward this dir's nlink). Files and other non-directories
            // among the children do not contribute. This matches the
            // canonical Unix directory-nlink semantics that `ls -l`
            // displays.
            //
            // Spec source: POSIX `<sys/stat.h>` directory link-count
            // convention (referenced by the EROFS read-side tools and by
            // every other Unix filesystem). Independent implementation.
            PlanKind::Dir { children } => children
                .iter()
                .filter(|(_, c)| matches!(plan[*c as usize].kind, PlanKind::Dir { .. }))
                .fold(2u32, |n, _| n.saturating_add(1)),
            _ => (*count).max(1),
        };
    }
    nlinks
}

/// Decide compact-vs-extended, build the inline-xattr blob, FLAT_INLINE
/// tail, and chunkmap size for a single inode. Mutates the FileLayout
/// choice on the PlanNode in place.
///
/// `lzma_dict_size_override` is forwarded to the LZMA encoder when
/// planning compressed inodes — it lets the writer advertise a
/// non-default dict in the COMPR_CFGS blob and patch the in-stream
/// LZMA1 header to match. `None` keeps the lzma-rs default (8 MiB).
fn plan_body(
    n: &mut PlanNode,
    nlink: u32,
    shared_xattrs: &[u32],
    bs: u64,
    lzma_dict_size_override: Option<u32>,
    options: &BuildOptions,
) -> Result<InodeBody> {
    let (xattr_bytes, xattr_icount) = encode_inline_xattrs(&n.xattrs, shared_xattrs)?;

    // Compute the file-size field that ends up in i_size. For dirs the
    // size depends on dir-block count and is filled in later (we report
    // 0 here since it doesn't affect compact-vs-extended choice -- a
    // directory never exceeds 4 GiB in our writer).
    let size = match &n.kind {
        PlanKind::File { size, .. } => *size,
        PlanKind::Symlink { target } => target.len() as u64,
        PlanKind::Dir { .. } => 0,
        PlanKind::Device { .. } | PlanKind::Special => 0,
        PlanKind::Chunked {
            chunks, chunk_bits, ..
        } => total_chunked_size(chunks, *chunk_bits, bs)?,
        PlanKind::Compressed { size, .. } => *size,
    };

    let is_extended = size > u32::MAX as u64
        || nlink > u16::MAX as u32
        || n.meta.uid > u16::MAX as u32
        || n.meta.gid > u16::MAX as u32
        // A compact inode has no mtime field: it reads as the
        // superblock's build time, so only a different mtime needs the
        // extended body.
        || n.meta.mtime != options.build_time
        || n.meta.mtime_nsec != options.build_time_nsec;

    // FLAT_INLINE for regular files whose entire payload fits in one
    // metadata block AFTER the body+xattrs. The block-fit invariant is
    // enforced again at NID-assignment time (the inode itself can land
    // anywhere in its meta block); here we only set the upper bound.
    // Spec: an inline tail must not cross a metadata-block boundary
    // (the public EROFS on-disk format imposes this constraint; fsck
    // enforces it on read).
    let body_size_now = if is_extended {
        EXTENDED_INODE_SIZE
    } else {
        COMPACT_INODE_SIZE
    };
    let mut inline_tail: Vec<u8> = Vec::new();
    if let PlanKind::File {
        content,
        size,
        layout,
    } = &mut n.kind
    {
        let tail_room = bs.saturating_sub(body_size_now + xattr_bytes.len() as u64);
        if *size > 0 && *size < bs && *size <= tail_room {
            *layout = FileLayout::FlatInline;
            inline_tail = content
                .take()
                .expect("a file's content is read once")
                .read_all()?;
        }
    }

    let chunkmap_bytes = if let PlanKind::Chunked {
        chunks,
        use_indexed_format,
        ..
    } = &n.kind
    {
        let entry = if *use_indexed_format { 8u64 } else { 4u64 };
        chunks.len() as u64 * entry
    } else {
        0
    };

    // Compression planning: bake the per-pcluster block bytes here so
    // pass 4 can write them at known offsets, and report the zmap
    // trailer size (header + index area + optional inline-tail) so
    // pass 2's block-fit check accounts for it the same way it does
    // for xattrs and chunkmaps.
    //
    // Spec: `z_erofs_map_header` definition in the public EROFS format
    // header `erofs_fs.h`; pack geometry in the public EROFS
    // compression-format documentation
    // (https://erofs.docs.kernel.org/en/latest/design.html#compressed-data).
    // Independent implementation.
    let mut zmap_bytes: u64 = 0;
    if let PlanKind::Compressed {
        src,
        algo,
        lclusterbits,
        pclusters,
        lcluster_entries,
        total_compressed_blocks,
        index_format,
        ztailpacking_requested,
        ztailpacking_inline,
        target_pcluster_blocks,
        ..
    } = &mut n.kind
    {
        let (mut pcs, lcl_entries, mut total) = plan_compressed_pclusters_with_cfg(
            &std::mem::take(src),
            *algo,
            *lclusterbits,
            bs,
            *target_pcluster_blocks,
            lzma_dict_size_override,
        )?;
        // n_lclusters drives index area sizing; with collation this
        // is independent of pcs.len().
        let n_lclusters = lcl_entries.len() as u64;
        let body_size_for_inode = if is_extended {
            EXTENDED_INODE_SIZE
        } else {
            COMPACT_INODE_SIZE
        };
        // body_end mod 32 is determined by (body_size + xattr_size)
        // mod 32: meta_blkaddr * bs is 32-aligned (bs is a power of 2
        // >= 512) and nid * 32 is also 32-aligned. So we can reason
        // about ebase % 32 here without knowing the final NID.
        let body_end_mod_32 = (body_size_for_inode + xattr_bytes.len() as u64) % 32;
        let ebase_mod_32 = body_end_mod_32.next_multiple_of(COMPACT_MAP_EBASE_ALIGN) % 32 + 8;
        let ebase_mod_32 = ebase_mod_32 % 32;

        // Index-area bytes (no header, just the packs / entries).
        let index_bytes = match *index_format {
            CompressedIndexFormat::Legacy => n_lclusters * Z_EROFS_LCLUSTER_INDEX_SIZE,
            CompressedIndexFormat::Compacted2B => {
                // Try with 2B-middle enabled; the geometry uses it
                // only when initial < totalidx and remaining >= 16.
                let totalidx = u32::try_from(n_lclusters)
                    .map_err(|_| Error::BadInode("compacted-2B totalidx overflow u32"))?;
                let geom = compute_compact_geom(ebase_mod_32, totalidx, true);
                geom.bytes()
            }
        };

        // Header byte size, with the padding in front of it: the header
        // sits at the first 8-byte boundary after body_end, where the
        // kernel reads it (`ALIGN(iloc + inode_isize + xattr_isize,
        // 8)`), so an xattr area that ends 4 bytes past a boundary is
        // followed by 4 bytes of zeros. Then legacy = 16 (8 struct + 8
        // reserved gap) and compact = 8. body_end mod 8 =
        // body_end_mod_32 % 8.
        let pad_before_header = (8 - (body_end_mod_32 % 8)) % 8;
        let header_plus_pad = pad_before_header
            + match *index_format {
                CompressedIndexFormat::Legacy => Z_EROFS_LEGACY_MAP_HEADER_SIZE,
                CompressedIndexFormat::Compacted2B => Z_EROFS_COMPACT_MAP_HEADER_SIZE,
            };

        // Decide ztailpacking: inline the LAST PCLUSTER's compressed
        // bytes if requested AND there's enough room left in the
        // metadata block AND we have at least one pcluster. Spec
        // note: ztailpacking is a single-pcluster optimisation; the
        // inline bytes go RIGHT after the index area. With
        // multi-lcluster pclusters, the LAST pcluster (by index)
        // owns the file's tail bytes regardless of how many
        // lclusters it spans.
        let mut idata_size: u64 = 0;
        if *ztailpacking_requested && !pcs.is_empty() {
            let last = pcs.last().expect("non-empty pcluster vec");
            let candidate_idata: u64 = last.raw_compressed.len() as u64;
            // idata_size is a u16 in the header union (high 16 bits).
            if candidate_idata <= u16::MAX as u64 {
                let trailer_with_inline = header_plus_pad + index_bytes + candidate_idata;
                let total_with_inline = body_size_for_inode
                    + xattr_bytes.len() as u64
                    + inline_tail.len() as u64
                    + chunkmap_bytes
                    + trailer_with_inline;
                if total_with_inline <= bs {
                    idata_size = candidate_idata;
                }
            }
        }

        if idata_size > 0 {
            // Steal the LAST pcluster's data block: its compressed
            // bytes will be inlined instead of written to a data block.
            let last_pcluster_idx = pcs.len() - 1;
            let inline_bytes = std::mem::take(&mut pcs[last_pcluster_idx].raw_compressed);
            let stolen_blocks = pcs[last_pcluster_idx].pcluster_block_count;
            pcs[last_pcluster_idx].block_bytes.clear();
            pcs[last_pcluster_idx].pcluster_block_count = 0;
            *ztailpacking_inline = Some(ZtailInline {
                bytes: inline_bytes,
            });
            // Adjust the total_compressed_blocks count: the inlined
            // pcluster doesn't consume a data block. (Multi-block
            // pclusters with target_pcluster_blocks > 1 would steal
            // more than 1 block, hence we use stolen_blocks rather
            // than a literal 1.)
            total = total.saturating_sub(stolen_blocks);
        }

        *pclusters = pcs;
        *lcluster_entries = lcl_entries;
        *total_compressed_blocks = total;
        zmap_bytes = header_plus_pad + index_bytes + idata_size;
    }

    let body_size = if is_extended {
        EXTENDED_INODE_SIZE
    } else {
        COMPACT_INODE_SIZE
    };

    Ok(InodeBody {
        body_size,
        is_extended,
        xattr_bytes,
        xattr_icount,
        inline_tail,
        chunkmap_bytes,
        zmap_bytes,
    })
}

fn total_chunked_size(chunks: &[Option<Vec<u8>>], chunk_bits: u8, bs: u64) -> Result<u64> {
    if chunks.is_empty() {
        return Ok(0);
    }
    let chunk_size = bs << chunk_bits;
    let mut total = 0u64;
    for (i, c) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        match c {
            Some(buf) => {
                if !last && (buf.len() as u64) != chunk_size {
                    return Err(Error::BadInode(
                        "chunked file: non-final chunk must be exactly chunk_size bytes",
                    ));
                }
                if buf.len() as u64 > chunk_size {
                    return Err(Error::BadInode(
                        "chunked file: chunk larger than chunk_size",
                    ));
                }
                total += buf.len() as u64;
            }
            None => {
                // Hole: treated as a full chunk of zeros, including in
                // the last position. This matches how mkfs.erofs sizes
                // sparse files.
                total += chunk_size;
            }
        }
    }
    Ok(total)
}

/// Encode the inline-xattr blob for a single inode. Returns
/// `(bytes, xattr_icount)`. The byte count satisfies the reader's
/// `body_end` math: `bytes.len() == 12 + (xattr_icount - 1) * 4`.
///
/// Spec: `linux/fs/erofs/xattr.h::erofs_xattr_ibody_size()`. Independent
/// implementation.
/// The shared xattr area and, by plan index, the references each inode
/// holds into it.
struct SharedXattrs {
    /// The entries, each 4-byte aligned, as they sit at `xattr_blkaddr`.
    area: Vec<u8>,
    /// Each inode's shared references: an entry's byte offset into
    /// `area`, divided by 4.
    refs: Vec<Vec<u32>>,
}

impl SharedXattrs {
    fn none(inodes: usize) -> Self {
        Self {
            area: Vec::new(),
            refs: vec![Vec::new(); inodes],
        }
    }
}

/// Move every xattr that two or more inodes carry into the shared area,
/// leaving each inode the ones only it has. Entries are placed in the
/// order the plan first meets them, so the same tree gives the same area.
fn share_repeated_xattrs(plan: &mut [PlanNode]) -> Result<SharedXattrs> {
    type Key = (u8, Vec<u8>, Vec<u8>);
    let key = |x: &XattrSpec| -> Key { (x.name_index, x.name.clone(), x.value.clone()) };
    let mut seen: BTreeMap<Key, (u32, Option<u32>)> = BTreeMap::new();
    for node in plan.iter() {
        for x in &node.xattrs {
            seen.entry(key(x)).or_insert((0, None)).0 += 1;
        }
    }
    let mut shared = SharedXattrs::none(plan.len());
    for (node, refs) in plan.iter_mut().zip(&mut shared.refs) {
        let mut inline = Vec::new();
        for x in std::mem::take(&mut node.xattrs) {
            let (count, id) = seen.get_mut(&key(&x)).expect("counted above");
            if *count < 2 {
                inline.push(x);
                continue;
            }
            let id = match id {
                Some(id) => *id,
                None => {
                    let at = u32::try_from(shared.area.len() / XATTR_ENTRY_ALIGN as usize)
                        .map_err(|_| Error::BadXattr("shared xattr area too large"))?;
                    push_xattr_entry(&mut shared.area, &x)?;
                    *id = Some(at);
                    at
                }
            };
            refs.push(id);
        }
        node.xattrs = inline;
    }
    Ok(shared)
}

/// Append one `erofs_xattr_entry` -- name length, namespace index, value
/// size, name, value -- padded to the 4-byte entry alignment. The inline
/// area and the shared area hold entries in the same form.
fn push_xattr_entry(buf: &mut Vec<u8>, x: &XattrSpec) -> Result<()> {
    if x.name.len() > u8::MAX as usize {
        return Err(Error::BadXattr("xattr name length > 255"));
    }
    if x.value.len() > u16::MAX as usize {
        return Err(Error::BadXattr("xattr value length > 65535"));
    }
    buf.push(x.name.len() as u8);
    buf.push(x.name_index);
    buf.extend_from_slice(&(x.value.len() as u16).to_le_bytes());
    buf.extend_from_slice(&x.name);
    buf.extend_from_slice(&x.value);
    while !(buf.len() as u64).is_multiple_of(XATTR_ENTRY_ALIGN) {
        buf.push(0);
    }
    Ok(())
}

fn encode_inline_xattrs(xattrs: &[XattrSpec], shared: &[u32]) -> Result<(Vec<u8>, u16)> {
    if xattrs.is_empty() && shared.is_empty() {
        return Ok((Vec::new(), 0));
    }
    // 12-byte erofs_xattr_ibody_header: 4-byte name_filter (zero --
    // unused hash filter) + 1-byte h_shared_count + 7 bytes reserved,
    // then the shared references, then the inline entries.
    let mut buf = vec![0u8; XATTR_HEADER_SIZE];
    buf[4] = u8::try_from(shared.len())
        .map_err(|_| Error::BadXattr("more than 255 shared xattrs on one inode"))?;
    for id in shared {
        buf.extend_from_slice(&id.to_le_bytes());
    }
    for x in xattrs {
        push_xattr_entry(&mut buf, x)?;
    }
    debug_assert_eq!((buf.len() - XATTR_HEADER_SIZE) % 4, 0);
    let icount_minus_one = (buf.len() - XATTR_HEADER_SIZE) / 4;
    let icount: u16 = (icount_minus_one + 1)
        .try_into()
        .map_err(|_| Error::BadXattr("xattr icount overflow"))?;
    Ok((buf, icount))
}

#[allow(clippy::too_many_arguments)]
fn write_superblock(
    img: &mut [u8],
    blkszbits: u8,
    n_inodes: u64,
    total_blocks: u32,
    meta_blkaddr: u32,
    feature_incompat: u32,
    u1: u16,
    xattr_prefix_count: u8,
    xattr_prefix_start: u32,
    xattr_blkaddr: u32,
    options: &BuildOptions,
) {
    // Every field below is placed through `superblock::offsets`, the
    // same table the parser reads — so a field cannot move on the write
    // side without moving on the read side.
    use crate::superblock::offsets as sb;
    let off = EROFS_SUPER_OFFSET as usize;
    let put = |img: &mut [u8], r: std::ops::Range<usize>, v: &[u8]| {
        img[off + r.start..off + r.end].copy_from_slice(v);
    };
    put(img, sb::MAGIC, &EROFS_SUPER_MAGIC_V1.to_le_bytes());
    img[off + sb::BLKSZBITS] = blkszbits;
    put(img, sb::ROOT_NID, &0u16.to_le_bytes());
    put(img, sb::INOS, &n_inodes.to_le_bytes());
    put(img, sb::BLOCKS, &total_blocks.to_le_bytes());
    put(img, sb::META_BLKADDR, &meta_blkaddr.to_le_bytes());
    put(img, sb::XATTR_BLKADDR, &xattr_blkaddr.to_le_bytes());
    put(img, sb::BUILD_TIME, &options.build_time.to_le_bytes());
    put(
        img,
        sb::BUILD_TIME_NSEC,
        &options.build_time_nsec.to_le_bytes(),
    );
    put(img, sb::UUID, &options.uuid);
    let label = b"rsmkfs";
    img[off + sb::VOLUME_NAME.start..off + sb::VOLUME_NAME.start + label.len()]
        .copy_from_slice(label);
    put(img, sb::FEATURE_INCOMPAT, &feature_incompat.to_le_bytes());
    // u1 (`available_compr_algs` when COMPR_CFGS is set, otherwise the
    // `lz4_max_distance` union arm — left zero for non-cfgs images).
    put(img, sb::U1, &u1.to_le_bytes());
    // Empty / 0 by default; non-zero only when the writer emits a
    // custom xattr prefix dictionary.
    img[off + sb::XATTR_PREFIX_COUNT] = xattr_prefix_count;
    put(
        img,
        sb::XATTR_PREFIX_START,
        &xattr_prefix_start.to_le_bytes(),
    );

    // feature_compat: advertise SB_CHKSUM and compute the CRC32C over
    // the bytes from EROFS_SUPER_OFFSET to the end of the block that
    // contains the SB, with the checksum field at 0x04..0x08 zeroed
    // during the calculation. For the default 4 KiB block the SB lives
    // in block 0 and the range is 3072 bytes (4096 - 1024); for
    // smaller blocks the SB lives in a later block and the range is
    // `block_size - (off % block_size)` bytes.
    //
    // Upstream erofs-utils (and the kernel verifier) uses CRC32C-
    // Castagnoli with init=0xFFFFFFFF and NO final XOR-out; the
    // `crc32c` crate computes the standard form (final XOR with
    // 0xFFFFFFFF), so we undo that XOR to match. The checksum field
    // is already zero in `img` here -- nothing has written into it --
    // so we can compute over `img` directly without a temporary copy.
    put(
        img,
        sb::FEATURE_COMPAT,
        &EROFS_FEATURE_COMPAT_SB_CHKSUM.to_le_bytes(),
    );
    let block_size = 1usize << blkszbits;
    let crc_len = block_size - off % block_size;
    let sb_to_block_end = &img[off..off + crc_len];
    let csum = crc32c::crc32c(sb_to_block_end) ^ 0xFFFF_FFFF;
    put(img, sb::CHECKSUM, &csum.to_le_bytes());
}

/// Encode one inode's body bytes (32 for compact, 64 for extended).
///
/// Spec: `linux/fs/erofs/erofs_fs.h::erofs_inode_compact` /
/// `erofs_inode_extended`. Independent implementation.
#[allow(clippy::too_many_arguments)]
fn encode_inode(
    n: &PlanNode,
    nid: u64,
    body: &InodeBody,
    nlink: u32,
    place: &Placement,
    bs: u64,
) -> Vec<u8> {
    let mut buf = vec![0u8; body.body_size as usize];

    let (layout, flags) = inode_layout_and_flags(&n.kind, body);
    let raw_format: u16 =
        (if body.is_extended { 1 } else { 0 }) | ((layout as u16) << 1) | ((flags & 0x0FFF) << 4);
    buf[ci::FORMAT].copy_from_slice(&raw_format.to_le_bytes());

    buf[ci::XATTR_ICOUNT].copy_from_slice(&body.xattr_icount.to_le_bytes());
    buf[ci::MODE].copy_from_slice(&n.mode.to_le_bytes());

    let raw_u = inode_raw_u(n, place);
    buf[ci::RAW_U].copy_from_slice(&raw_u.to_le_bytes());

    let size = match &n.kind {
        PlanKind::Dir { .. } => place.dir_size,
        PlanKind::File { size, .. } => *size,
        PlanKind::Symlink { target } => target.len() as u64,
        PlanKind::Device { .. } | PlanKind::Special => 0,
        PlanKind::Chunked {
            chunks, chunk_bits, ..
        } => total_chunked_size(chunks, *chunk_bits, bs).unwrap_or(0),
        // Compressed: i_size is the ORIGINAL (uncompressed) source
        // length. The reader uses it to bound the last pcluster's
        // decompressed-source span.
        PlanKind::Compressed { size, .. } => *size,
    };

    if body.is_extended {
        buf[xi::SIZE].copy_from_slice(&size.to_le_bytes());
        buf[xi::INO].copy_from_slice(&((nid as u32).wrapping_add(1)).to_le_bytes());
        buf[xi::UID].copy_from_slice(&n.meta.uid.to_le_bytes());
        buf[xi::GID].copy_from_slice(&n.meta.gid.to_le_bytes());
        buf[xi::MTIME].copy_from_slice(&n.meta.mtime.to_le_bytes());
        buf[xi::MTIME_NSEC].copy_from_slice(&n.meta.mtime_nsec.to_le_bytes());
        buf[xi::NLINK].copy_from_slice(&nlink.to_le_bytes());
    } else {
        buf[ci::NLINK].copy_from_slice(&(nlink as u16).to_le_bytes());
        buf[ci::SIZE].copy_from_slice(&(size as u32).to_le_bytes());
        buf[ci::INO].copy_from_slice(&((nid as u32).wrapping_add(1)).to_le_bytes());
        buf[ci::UID].copy_from_slice(&(n.meta.uid as u16).to_le_bytes());
        buf[ci::GID].copy_from_slice(&(n.meta.gid as u16).to_le_bytes());
    }

    buf
}

fn inode_layout_and_flags(kind: &PlanKind, body: &InodeBody) -> (DataLayout, u16) {
    match kind {
        PlanKind::File { layout, .. } => match layout {
            FileLayout::FlatPlain => (DataLayout::FlatPlain, 0),
            FileLayout::FlatInline => (DataLayout::FlatInline, 0),
        },
        // A directory or symlink whose last block is inline.
        PlanKind::Dir { .. } | PlanKind::Symlink { .. } if !body.inline_tail.is_empty() => {
            (DataLayout::FlatInline, 0)
        }
        PlanKind::Dir { .. }
        | PlanKind::Symlink { .. }
        | PlanKind::Device { .. }
        | PlanKind::Special => (DataLayout::FlatPlain, 0),
        // Chunked: per spec, the chunk format lives in i_u (handled in
        // `inode_raw_u`), NOT in the i_format flags. fsck.erofs rejects
        // i_format values with high bits set, so we keep flags zero here.
        PlanKind::Chunked { .. } => (DataLayout::ChunkBased, 0),
        // Compressed: layout id depends on `index_format`. Legacy (1)
        // routes the reader to the 8-byte-per-lcluster
        // `z_erofs_lcluster_index` format; Compression (3) routes it
        // to the compacted-2B pack format. The low 4 bits of the
        // format-flags nibble carry `lclusterbits`. Spec:
        // `linux/fs/erofs/erofs_fs.h::EROFS_I_COMPRESSED_BIT` +
        // `EROFS_INODE_COMPRESSED_FULL` / `EROFS_INODE_COMPRESSED_COMPACT`.
        // Independent implementation.
        PlanKind::Compressed {
            lclusterbits,
            index_format,
            ..
        } => {
            let layout = match index_format {
                CompressedIndexFormat::Legacy => DataLayout::CompressionLegacy,
                CompressedIndexFormat::Compacted2B => DataLayout::Compression,
            };
            (layout, (*lclusterbits as u16) & 0x000F)
        }
    }
}

fn inode_raw_u(n: &PlanNode, place: &Placement) -> u32 {
    match &n.kind {
        PlanKind::File { layout, .. } => match layout {
            // FLAT_INLINE: the whole blocks before the inline tail, if any.
            FileLayout::FlatPlain | FileLayout::FlatInline => place.blkaddr as u32,
        },
        PlanKind::Dir { .. } | PlanKind::Symlink { .. } => place.blkaddr as u32,
        PlanKind::Device { rdev } => *rdev,
        PlanKind::Special => 0,
        PlanKind::Chunked {
            chunk_bits,
            use_indexed_format,
            ..
        } => {
            // Spec: for ChunkBased layout, i_u carries the chunk_format
            // (low 16 bits): chunk_bits in bits 0..=4 and the INDEXES
            // bit at position 5. The high 16 bits are reserved.
            // Source: `linux/fs/erofs/erofs_fs.h::erofs_inode_chunk_info`.
            // Independent implementation.
            let mut cf: u16 = (*chunk_bits as u16) & EROFS_CHUNK_FORMAT_BLKBITS_MASK;
            if *use_indexed_format {
                cf |= EROFS_CHUNK_FORMAT_INDEXES;
            }
            cf as u32
        }
        // Spec: for compressed inodes, the i_u union carries
        // `compressed_blocks` -- the count of pcluster blocks in the
        // data area, used by the reader to bound the last pcluster's
        // on-disk size. Source: `linux/fs/erofs/erofs_fs.h` (the i_u
        // union doc) + `zmap.rs::n_pclusters`. Independent
        // implementation.
        PlanKind::Compressed {
            total_compressed_blocks,
            ..
        } => *total_compressed_blocks,
    }
}

/// Pack a directory's entries into `block_size`-sized blocks. ".", ".."
/// always anchor block 0; subsequent children are sorted byte-wise by
/// name (`memcmp` order) and then greedily packed, overflowing into
/// block 1, 2, ... as needed. Each block's dirent array is internally
/// consistent (its own first nameoff = its array end), so the reader
/// can iterate per-block.
///
/// Why byte-wise sort: upstream `fsck.erofs` enforces strict
/// `strcmp`-ascending order on dirents within a block (errors out with
/// "wrong dirent name order"), and the kernel `erofs_namei` lookup
/// does a linear scan that requires the same order. Hash-keyed
/// binary-search ordering does NOT exist in the on-disk format -- the
/// `erofs_dirent` struct has no hash field; lookups are
/// strcmp-based against the name table.
///
/// Spec: `erofs_dirent` byte format in the public EROFS format header
/// `erofs_fs.h`; ordering convention conveyed by the public EROFS
/// directory-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#directory-format>).
/// Independent implementation.
fn encode_dir_blocks(
    parent_idx: usize,
    children: &[(String, u64)],
    plan: &[PlanNode],
    nids: &[u64],
    bs: u64,
) -> Result<(Vec<Vec<u8>>, usize)> {
    let parent = &plan[parent_idx];
    let parent_nid = nids[parent_idx];
    let parent_parent_nid = nids[parent.parent_idx as usize];

    let mut all: Vec<(String, u64, u8)> = Vec::with_capacity(2 + children.len());
    all.push((".".to_string(), parent_nid, ftype::DIR));
    all.push(("..".to_string(), parent_parent_nid, ftype::DIR));

    // Sort the real children byte-wise by name. ".", ".." stay at
    // indexes 0/1 by EROFS convention; they're not part of the sorted
    // run. fsck.erofs enforces strict strcmp-ascending order on the
    // remaining entries within a block.
    let mut sorted_children: Vec<(String, u64, u8)> = children
        .iter()
        .map(|(name, child_idx)| {
            let child = &plan[*child_idx as usize];
            let ft = file_type_byte(child);
            (name.clone(), nids[*child_idx as usize], ft)
        })
        .collect();
    sorted_children.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    all.extend(sorted_children);

    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut last_used = 0usize;
    let mut start = 0usize;
    while start < all.len() {
        let mut end = start;
        let mut header = 0usize;
        let mut names = 0usize;
        while end < all.len() {
            let nh = header + EROFS_DIRENT_SIZE;
            let nn = names + all[end].0.len();
            if (nh + nn) as u64 > bs {
                if end == start {
                    return Err(Error::BadDirent("single dir entry larger than block_size"));
                }
                break;
            }
            header = nh;
            names = nn;
            end += 1;
        }
        let block = encode_one_dir_block(&all[start..end], bs)?;
        out.push(block);
        last_used = header + names;
        start = end;
    }
    if out.is_empty() {
        return Err(Error::BadDirent("dir has zero entries"));
    }
    Ok((out, last_used))
}

fn encode_one_dir_block(entries: &[(String, u64, u8)], bs: u64) -> Result<Vec<u8>> {
    let n = entries.len();
    let header_size = n * EROFS_DIRENT_SIZE;
    let names_size: usize = entries.iter().map(|(name, _, _)| name.len()).sum();
    if (header_size + names_size) as u64 > bs {
        return Err(Error::BadDirent("dir block overflow (planner bug)"));
    }
    let mut buf = vec![0u8; bs as usize];
    let mut name_cursor = header_size;
    for (i, (name, nid_ref, ft)) in entries.iter().enumerate() {
        let off = i * EROFS_DIRENT_SIZE;
        buf[off..off + 8].copy_from_slice(&nid_ref.to_le_bytes());
        buf[off + 8..off + 10].copy_from_slice(&(name_cursor as u16).to_le_bytes());
        buf[off + 10] = *ft;
        buf[name_cursor..name_cursor + name.len()].copy_from_slice(name.as_bytes());
        name_cursor += name.len();
    }
    Ok(buf)
}

fn file_type_byte(n: &PlanNode) -> u8 {
    match &n.kind {
        PlanKind::File { .. } | PlanKind::Chunked { .. } | PlanKind::Compressed { .. } => {
            ftype::REG_FILE
        }
        PlanKind::Dir { .. } => ftype::DIR,
        PlanKind::Symlink { .. } => ftype::SYMLINK,
        // A device or special node's type comes from its mode, which
        // is the same question `dirent_type_for_mode` answers. The
        // `PlanKind` narrows what is acceptable — a Device that is not
        // a char or block device is a planning error, not a FIFO — so
        // the result is filtered rather than trusted wholesale.
        PlanKind::Device { .. } => match crate::dir::dirent_type_for_mode(n.mode) {
            t @ (ftype::CHRDEV | ftype::BLKDEV) => t,
            _ => ftype::UNKNOWN,
        },
        PlanKind::Special => match crate::dir::dirent_type_for_mode(n.mode) {
            t @ (ftype::FIFO | ftype::SOCK) => t,
            _ => ftype::UNKNOWN,
        },
    }
}

/// Encode the zmap trailer (header + index area + optional inline
/// ztailpack bytes) for a compressed inode. Dispatches on
/// `index_format`:
///
/// - [`CompressedIndexFormat::Legacy`]: 16-byte combined header
///   (8 struct + 8 reserved gap), followed by an array of 8-byte
///   `z_erofs_lcluster_index` entries.
/// - [`CompressedIndexFormat::Compacted2B`]: 8-byte struct header
///   (no gap), followed by 0..4 bytes of pad to 8-align body_end,
///   followed by 32-byte-aligned packs (each pack: bitstream of
///   `vcnt` entries plus a `__le32` per-pack base blkaddr).
///
/// `body_end` is the absolute on-disk byte offset where the header
/// will be written; it determines the compact-2B `ebase` (= ALIGN
/// (body_end, 8) + 8) and thus the `compacted_4b_initial` pack count.
///
/// Spec: `z_erofs_map_header` and `z_erofs_lcluster_index` in the
/// public EROFS format header `erofs_fs.h`; pack geometry in the public
/// EROFS compression-format documentation
/// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
/// Independent implementation. Not derived from any GPL'd EROFS
/// codebase.
fn encode_zmap_trailer(n: &PlanNode, addrs: &[u32], body_end: u64, blkszbits: u8) -> Vec<u8> {
    let PlanKind::Compressed {
        algo,
        lclusterbits,
        pclusters,
        lcluster_entries,
        index_format,
        ztailpacking_inline,
        big_pcluster,
        ..
    } = &n.kind
    else {
        return Vec::new();
    };
    let algorithm: u8 = match algo {
        CompressedAlgo::Lz4 => 0,
        CompressedAlgo::Lzma => 1,
        CompressedAlgo::Deflate => 2,
    };
    debug_assert_eq!(addrs.len(), pclusters.len());

    let has_inline = ztailpacking_inline.is_some();
    let idata_size: u16 = ztailpacking_inline
        .as_ref()
        .map(|t| t.bytes.len() as u16)
        .unwrap_or(0);

    match index_format {
        CompressedIndexFormat::Legacy => {
            // advise: ztailpacking only when inline is present.
            let mut advise = if has_inline {
                Z_EROFS_ADVISE_INLINE_PCLUSTER
            } else {
                0
            };
            if *big_pcluster {
                advise |= Z_EROFS_ADVISE_BIG_PCLUSTER_1;
            }
            let mut header = encode_zmap_header(advise, *lclusterbits, algorithm);
            // h_idata_size lives in the HIGH 16 bits of the
            // fragment_off / idata_size union (kernel
            // `z_erofs_map_header` overlays `__le32 h_fragmentoff`
            // with `{__le16 h_reserved1; __le16 h_idata_size;}`).
            if has_inline {
                let frag_or_idata = (idata_size as u32) << 16;
                header[0x00..0x04].copy_from_slice(&frag_or_idata.to_le_bytes());
            }
            // Zeros up to the 8-byte boundary the header sits on.
            let mut out = vec![0u8; ((8 - (body_end % 8)) % 8) as usize];
            out.reserve(
                Z_EROFS_LEGACY_MAP_HEADER_SIZE as usize
                    + lcluster_entries.len() * Z_EROFS_LCLUSTER_INDEX_SIZE as usize
                    + idata_size as usize,
            );
            out.extend_from_slice(&header);
            // One 8-byte entry per LOGICAL cluster. HEAD/PLAIN
            // entries carry their pcluster's blkaddr (resolved via
            // `pcluster_idx`); NONHEAD entries carry `delta[0]`, the
            // lclusters back to their HEAD, in the low 16 bits of the
            // `u` union, and `delta[1]`, the lclusters on to the next
            // HEAD or past the last lcluster, in the high 16. With big
            // pclusters, a HEAD1's first NONHEAD carries the pcluster's
            // block count in place of its `delta[0]`, which is 1 by
            // position, under the `Z_EROFS_LI_D0_CBLKCNT` flag.
            let delta1 = lcluster_delta1(lcluster_entries);
            for (i, entry) in lcluster_entries.iter().enumerate() {
                let (clusterofs, u) = if entry.cluster_type == Z_EROFS_LCLUSTER_TYPE_NONHEAD {
                    let owner = &pclusters[entry.pcluster_idx as usize];
                    let delta0 = if *big_pcluster
                        && entry.clusterofs_or_delta0 == 1
                        && lcluster_entries[i - 1].cluster_type == Z_EROFS_LCLUSTER_TYPE_HEAD1
                    {
                        Z_EROFS_LI_D0_CBLKCNT | owner.pcluster_block_count
                    } else {
                        entry.clusterofs_or_delta0 as u32
                    };
                    (0u16, delta0 | (delta1[i] as u32) << 16)
                } else {
                    let blkaddr = addrs[entry.pcluster_idx as usize];
                    (entry.clusterofs_or_delta0, blkaddr)
                };
                let bytes = encode_lcluster_index(entry.cluster_type, clusterofs, u);
                out.extend_from_slice(&bytes);
            }
            if let Some(t) = ztailpacking_inline {
                out.extend_from_slice(&t.bytes);
            }
            out
        }
        CompressedIndexFormat::Compacted2B => {
            // Compute geometry from the actual ebase. The reader
            // does the same, so we MUST mirror it byte for byte.
            let ebase = body_end.next_multiple_of(COMPACT_MAP_EBASE_ALIGN)
                + Z_EROFS_COMPACT_MAP_HEADER_SIZE;
            let totalidx = lcluster_entries.len() as u32;
            // For totalidx == 0 the geometry is trivially empty; for
            // totalidx >= 16 we'd benefit from the 2B middle region.
            // Setting `want_2b_middle = true` always is safe because
            // `compute_compact_geom` falls back to 4B-only when there
            // aren't enough lclusters to fill a 16-entry 2B pack.
            let geom = compute_compact_geom(ebase, totalidx, true);
            let lobits = (blkszbits as u32 + (*lclusterbits as u32)).max(12);

            // Build the entries array. NONHEAD entries put `delta[0]`
            // in `lo`; HEAD/PLAIN entries put `clusterofs` in `lo`.
            // The pcluster_blkaddr is consulted only for HEAD/PLAIN
            // (NONHEAD entries don't contribute to per-pack base
            // arithmetic).
            let mut entries: Vec<CompactEntry> = Vec::with_capacity(totalidx as usize);
            for e in lcluster_entries {
                let (lo, blkaddr) = if e.cluster_type == Z_EROFS_LCLUSTER_TYPE_NONHEAD {
                    (e.clusterofs_or_delta0 as u32, 0u32)
                } else {
                    let blkaddr = addrs[e.pcluster_idx as usize];
                    (e.clusterofs_or_delta0 as u32, blkaddr)
                };
                entries.push(CompactEntry {
                    cluster_type: e.cluster_type,
                    lo,
                    pcluster_blkaddr: blkaddr,
                });
            }

            // Header. Advise: COMPACTED_2B if the 2B middle was
            // actually used, plus INLINE_PCLUSTER for ztailpacking.
            let mut advise: u16 = 0;
            if geom.use_2b_middle {
                advise |= Z_EROFS_ADVISE_COMPACTED_2B;
            }
            if has_inline {
                advise |= Z_EROFS_ADVISE_INLINE_PCLUSTER;
            }
            // Compact uses an 8-byte struct header (no reserved gap).
            // We synthesise it directly to avoid the legacy 16-byte
            // helper's gap.
            let mut header = [0u8; 8];
            let frag_or_idata = (idata_size as u32) << 16;
            header[0x00..0x04].copy_from_slice(&frag_or_idata.to_le_bytes());
            header[0x04..0x06].copy_from_slice(&advise.to_le_bytes());
            // Per the public spec: byte 6 = h_algorithmtype (low4 =
            // HEAD1 algo), byte 7 = h_clusterbits (low4 = lclusterbits).
            header[0x06] = algorithm & 0x0F;
            header[0x07] = (*lclusterbits) & 0x0F;

            // Pad-to-8 between header and ebase (aligns body_end up
            // to 8). Header bytes occupy [body_end, body_end+8); the
            // alignment-up-to-8 of body_end is ALIGN(body_end, 8),
            // and ebase = ALIGN(body_end, 8) + 8. So the padding is
            // (8 - body_end % 8) mod 8 bytes, inserted BETWEEN the
            // 8-byte header and the index area.
            // Zeros up to the 8-byte boundary the header sits on.
            let pad_before_header = ((8 - (body_end % 8)) % 8) as usize;

            let index_bytes = encode_compact2b_index(&geom, &entries, lobits);

            let mut out = vec![0u8; pad_before_header];
            out.reserve(8 + index_bytes.len() + idata_size as usize);
            out.extend_from_slice(&header);
            out.extend_from_slice(&index_bytes);
            if let Some(t) = ztailpacking_inline {
                out.extend_from_slice(&t.bytes);
            }
            out
        }
    }
}

/// Encode one chunkmap entry array. Returns the on-disk bytes.
///
/// Spec: `linux/fs/erofs/erofs_fs.h::erofs_inode_chunk_index` (8 bytes
/// indexed) or bare `__le32` (4 bytes compact). Independent implementation.
fn encode_chunkmap(n: &PlanNode, addrs: &[u32]) -> Vec<u8> {
    let PlanKind::Chunked {
        use_indexed_format, ..
    } = &n.kind
    else {
        return Vec::new();
    };
    if *use_indexed_format {
        let mut buf = Vec::with_capacity(addrs.len() * 8);
        for a in addrs {
            buf.extend_from_slice(&0u16.to_le_bytes()); // advise
            buf.extend_from_slice(&0u16.to_le_bytes()); // device_id
            buf.extend_from_slice(&a.to_le_bytes());
        }
        buf
    } else {
        let mut buf = Vec::with_capacity(addrs.len() * 4);
        for a in addrs {
            buf.extend_from_slice(&a.to_le_bytes());
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_device::MemDev;
    use crate::Filesystem;
    use fs_core::BlockRead;
    use std::sync::Arc;

    fn dir(entries: Vec<(&str, Node)>) -> Node {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            m.insert(k.to_string(), v);
        }
        Node::Dir {
            mode: DEFAULT_DIR_MODE,
            entries: m,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        }
    }
    fn file(data: &[u8]) -> Node {
        Node::File {
            mode: DEFAULT_FILE_MODE,
            data: data.to_vec(),
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        }
    }

    fn open(img: Vec<u8>) -> Filesystem {
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev::new(img));
        Filesystem::open(dev).unwrap()
    }

    #[test]
    fn empty_dir_image_round_trips() {
        let img = build_image(dir(vec![]), 12).unwrap();
        let fs = open(img);
        let root = fs.root_inode().unwrap();
        assert!(root.is_dir());
        let entries = fs.read_dir(&root).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b".");
        assert_eq!(entries[1].name, b"..");
    }

    #[test]
    fn one_file_round_trips() {
        let img = build_image(dir(vec![("hello.txt", file(b"hi there\n"))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/hello.txt").unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size, 9);
        let mut buf = vec![0u8; 9];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, b"hi there\n");
    }

    #[test]
    fn nested_dirs_and_multi_block_file() {
        let big: Vec<u8> = (0..200_000u32).map(|i| (i & 0xFF) as u8).collect();
        let img = build_image(
            dir(vec![
                ("sub", dir(vec![("nested.txt", file(b"nested\n"))])),
                ("big.bin", file(&big)),
            ]),
            12,
        )
        .unwrap();
        let fs = open(img);

        let inode = fs.lookup_path("/big.bin").unwrap();
        assert_eq!(inode.size, 200_000);
        let mut buf = vec![0u8; 200_000];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, big);

        let nested = fs.lookup_path("/sub/nested.txt").unwrap();
        let mut buf = vec![0u8; 7];
        fs.read_file(&nested, 0, &mut buf).unwrap();
        assert_eq!(buf, b"nested\n");
    }

    #[test]
    fn empty_file_works() {
        let img = build_image(dir(vec![("empty.txt", file(b""))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/empty.txt").unwrap();
        assert_eq!(inode.size, 0);
        let mut buf = [0u8; 0];
        fs.read_file(&inode, 0, &mut buf).unwrap();
    }

    #[test]
    fn rejects_invalid_name() {
        let img = build_image(dir(vec![("bad/name", file(b""))]), 12);
        assert!(matches!(img, Err(Error::BadDirent(_))));
    }

    #[test]
    fn invalid_blkszbits_rejected() {
        assert!(build_image(dir(vec![]), 5).is_err());
        assert!(build_image(dir(vec![]), 20).is_err());
    }

    #[test]
    fn small_file_emits_flat_inline() {
        let img = build_image(dir(vec![("tiny.bin", file(b"tiny payload"))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/tiny.bin").unwrap();
        assert_eq!(inode.format.layout, DataLayout::FlatInline);
        let mut buf = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, b"tiny payload");
    }

    #[test]
    fn extended_inode_promotion_for_large_uid() {
        let n = Node::File {
            mode: DEFAULT_FILE_MODE,
            data: b"hi".to_vec(),
            meta: NodeMeta {
                uid: 70_000,
                ..Default::default()
            },
            xattrs: Vec::new(),
        };
        let img = build_image(dir(vec![("a.txt", n)]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/a.txt").unwrap();
        assert_eq!(inode.on_disk_size, 64);
        assert_eq!(inode.uid, 70_000);
    }

    // --- W2a: LZ4 compressed-file round-trip tests -----------------------

    fn compressed(data: &[u8]) -> Node {
        Node::CompressedFile(CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: data.to_vec(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedFileSpec::default_index_format(),
            ztailpacking: false,
            target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
        })
    }

    /// Helper that builds a [`Node::CompressedFile`] with the modern
    /// compacted-2B index format. Used by the W2b tests below.
    fn compressed_compacted2b(data: &[u8], ztailpacking: bool) -> Node {
        Node::CompressedFile(CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: data.to_vec(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedIndexFormat::Compacted2B,
            ztailpacking,
            target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
        })
    }

    #[test]
    fn compressed_lz4_small_file() {
        // Smaller-than-lcluster file: a single HEAD1 entry (or PLAIN
        // fallback if the LZ4 frame doesn't shrink it).
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let img = build_image(dir(vec![("c.bin", compressed(&payload))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_lz4_multi_lcluster() {
        // 5 lclusters at default lclusterbits=0 with 4 KiB blocks ->
        // 5 separate pclusters under our W2a one-per-lcluster policy.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'a'; 5 * bs];
        let img = build_image(dir(vec![("c.bin", compressed(&payload))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_lz4_incompressible() {
        // Random-looking bytes (an LCG sequence) so LZ4 can't find
        // matches and the compressed output >= source. PLAIN
        // passthrough must engage.
        let bs: usize = 4096;
        let n = 3 * bs;
        let mut payload = Vec::with_capacity(n);
        let mut state: u64 = 0xdead_beef_cafe_babe;
        for _ in 0..n {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            payload.push((state >> 56) as u8);
        }
        let img = build_image(dir(vec![("r.bin", compressed(&payload))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/r.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_lz4_partial_last_lcluster() {
        // 3.5 lclusters: file_size % lcluster_size != 0. The reader
        // bounds the last pcluster by `inode.size`, so a partial
        // decompression of the trailing lcluster must still match.
        let bs: usize = 4096;
        let n = 3 * bs + bs / 2;
        let payload: Vec<u8> = (0..n as u32).map(|i| (i & 0xFF) as u8).collect();
        let img = build_image(dir(vec![("p.bin", compressed(&payload))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/p.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_lz4_with_xattrs() {
        // Compression + inline xattrs in the same inode: the zmap
        // header must land at body_end (which now includes the xattr
        // ibody size). If body_end math drifts, the reader will parse
        // garbage for advise/algorithm/lclusterbits and decompression
        // explodes.
        use crate::xattr::ns;
        let payload = b"hello compressed xattrs\n".repeat(50);
        let n = Node::CompressedFile(CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.clone(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: vec![
                XattrSpec::new(ns::USER, b"k".to_vec(), b"v".to_vec()),
                XattrSpec::new(ns::TRUSTED, b"meta".to_vec(), b"x".to_vec()),
            ],
            index_format: CompressedFileSpec::default_index_format(),
            ztailpacking: false,
            target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
        });
        let img = build_image(dir(vec![("c.bin", n)]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert!(inode.xattr_icount > 0);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    // --- W2b: compacted-2B + ztailpacking round-trip tests ---------------

    #[test]
    fn compressed_compacted2b_small_file() {
        // Single-lcluster file in compacted-2B form. With totalidx=1
        // the geometry has `initial=1, middle=0, tail=0` (single 4B
        // pack of 8 bytes). Ztailpacking off so the lcluster owns a
        // real pcluster block.
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let img = build_image(
            dir(vec![("c.bin", compressed_compacted2b(&payload, false))]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size as usize, payload.len());
        // Layout id 3 == DataLayout::Compression (modern compact).
        assert_eq!(inode.format.layout, DataLayout::Compression);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_compacted2b_multi_pack() {
        // 12 lclusters at default lclusterbits=0 with 4 KiB blocks ->
        // 12 entries. Geometry: ebase = ALIGN(body_end, 8) + 8.
        // body_end ends at a 32-byte slot boundary (no xattrs); for
        // compact-32B inode body_end mod 32 = 0, so ebase mod 32 = 8,
        // initial = (32-8)/4 & 7 = 6. With 12 entries: initial=6,
        // middle=0 (remaining=6 < 16), tail=6 -> 3 initial 4B packs +
        // 3 tail 4B packs = 6 packs total, all 4B form.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'a'; 12 * bs];
        let img = build_image(
            dir(vec![("c.bin", compressed_compacted2b(&payload, false))]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::Compression);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_compacted2b_with_2b_middle_region() {
        // Enough lclusters to fill the 2B middle region. 22 lclusters:
        // initial=6, middle=rounddown(22-6, 16)=16, tail=0. Exercises
        // the COMPACTED_2B advise bit and the 14-bit-per-entry
        // bitstream encoding.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'b'; 22 * bs];
        let img = build_image(
            dir(vec![("c.bin", compressed_compacted2b(&payload, false))]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::Compression);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_compacted2b_with_ztailpacking() {
        // Small payload + ztailpacking on. A single lcluster's
        // compressed bytes get inlined past the index area in the
        // metadata block. The reader's `tail_inline_offset_and_size`
        // returns the (offset, size) and `read_compressed_block`
        // dispatches to the inline-tail path.
        let payload = b"hello compressed inline tail bytes pattern\n".repeat(10);
        let img = build_image(
            dir(vec![("c.bin", compressed_compacted2b(&payload, true))]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::Compression);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_compacted2b_partial_last_lcluster() {
        // 3.5 lclusters: file_size % lcluster_size != 0. The last
        // lcluster has a residual `size - 3*bs` source bytes.
        let bs: usize = 4096;
        let n = 3 * bs + bs / 2;
        let payload: Vec<u8> = (0..n as u32).map(|i| (i & 0xFF) as u8).collect();
        let img = build_image(
            dir(vec![("p.bin", compressed_compacted2b(&payload, false))]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/p.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_compacted2b_incompressible_pad() {
        // PLAIN passthrough on every lcluster, compacted-2B form. The
        // entries record cluster_type=PLAIN and the reader's PLAIN
        // dispatch passes blocks through. Confirms the bitstream-
        // decoding path treats PLAIN (cluster_type=0) correctly even
        // when wedged into 14-bit entries.
        let bs: usize = 4096;
        let n = 3 * bs;
        let mut payload = Vec::with_capacity(n);
        let mut state: u64 = 0xdead_beef_cafe_babe;
        for _ in 0..n {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            payload.push((state >> 56) as u8);
        }
        let img = build_image(
            dir(vec![("r.bin", compressed_compacted2b(&payload, false))]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/r.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_compacted2b_multiple_files_share_metadata_blocks() {
        // Two compacted-2B compressed files in the same image. Their
        // body_end's land at different mod-32 residues, exercising the
        // pad-before-index alignment path for the second inode.
        let p1 = b"first file payload\n".repeat(40);
        let p2 = b"second file payload differs\n".repeat(60);
        let img = build_image(
            dir(vec![
                ("a.bin", compressed_compacted2b(&p1, false)),
                ("b.bin", compressed_compacted2b(&p2, false)),
            ]),
            12,
        )
        .unwrap();
        let fs = open(img);
        for (name, want) in [("a.bin", &p1[..]), ("b.bin", &p2[..])] {
            let inode = fs.lookup_path(&format!("/{name}")).unwrap();
            assert_eq!(inode.size as usize, want.len());
            let mut buf = vec![0u8; want.len()];
            fs.read_file(&inode, 0, &mut buf).unwrap();
            assert_eq!(buf, want);
        }
    }

    // --- W3: LZMA + DEFLATE codec round-trip tests ----------------------

    /// Helper: compressed-file Node with the requested codec, legacy
    /// index format, default lclusterbits.
    fn compressed_with(algo: CompressedAlgo, data: &[u8]) -> Node {
        Node::CompressedFile(CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: data.to_vec(),
            algo,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedFileSpec::default_index_format(),
            ztailpacking: false,
            target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
        })
    }

    /// One `read_file` over a compressed file opens its index once, not
    /// once per block: this file is 48 blocks, and each used to reopen
    /// and re-parse the map header (#60). A directory listing and a
    /// lookup walk blocks the same way and share the hoist.
    #[test]
    fn a_compressed_read_opens_its_index_once() {
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(4460);
        let img = build_image(
            dir(vec![(
                "big.txt",
                compressed_with(CompressedAlgo::Lz4, &payload),
            )]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/big.txt").unwrap();
        assert!(inode.size.div_ceil(4096) > 40, "fixture: many blocks");
        let before = crate::zmap::OPENS.with(|n| n.get());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
        assert_eq!(
            crate::zmap::OPENS.with(|n| n.get()) - before,
            1,
            "the compressed index was opened more than once for one read"
        );

        // And an empty read opens nothing at all.
        let before = crate::zmap::OPENS.with(|n| n.get());
        fs.read_file(&inode, 0, &mut []).unwrap();
        assert_eq!(
            crate::zmap::OPENS.with(|n| n.get()),
            before,
            "an empty read opened the compressed index"
        );
    }

    #[test]
    fn compressed_lzma_small_file() {
        // Smaller-than-lcluster payload encoded with LZMA1. The
        // algorithm_type byte = 1 routes the reader to
        // `decompress::Algorithm::Lzma`.
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let img = build_image(
            dir(vec![(
                "c.bin",
                compressed_with(CompressedAlgo::Lzma, &payload),
            )]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_lzma_multi_lcluster() {
        // Multiple lclusters, all highly compressible -> HEAD1 on every
        // lcluster, codec = LZMA. Confirms the multi-pcluster reader
        // dispatch wires algorithm 1 through correctly.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'a'; 5 * bs];
        let img = build_image(
            dir(vec![(
                "c.bin",
                compressed_with(CompressedAlgo::Lzma, &payload),
            )]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_deflate_small_file() {
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let img = build_image(
            dir(vec![(
                "c.bin",
                compressed_with(CompressedAlgo::Deflate, &payload),
            )]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_deflate_multi_lcluster() {
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'a'; 5 * bs];
        let img = build_image(
            dir(vec![(
                "c.bin",
                compressed_with(CompressedAlgo::Deflate, &payload),
            )]),
            12,
        )
        .unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    // --- W2c: multi-lcluster pcluster collation -------------------------

    /// Helper: collated LZ4 compressed file. Same defaults as
    /// [`compressed`] but parameterised on `target_pcluster_blocks`.
    fn collated_lz4(
        data: &[u8],
        target_pcluster_blocks: u32,
        index_format: CompressedIndexFormat,
    ) -> Node {
        Node::CompressedFile(CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: data.to_vec(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format,
            ztailpacking: false,
            target_pcluster_blocks,
        })
    }

    /// Inspect the planned compressed inode for a single-file image:
    /// (n_pclusters, n_lclusters). Drives off the planner directly so
    /// the test can assert on layout decisions, not just round-trip.
    fn count_pclusters_and_lclusters(
        data: &[u8],
        spec: CompressedFileSpec,
        bs: u64,
    ) -> (usize, usize) {
        let (pcs, lcl, _total) = plan_compressed_pclusters(
            data,
            spec.algo,
            spec.lclusterbits,
            bs,
            spec.target_pcluster_blocks,
        )
        .expect("plan_compressed_pclusters");
        (pcs.len(), lcl.len())
    }

    #[test]
    fn compressed_collation_two_lclusters_one_pcluster() {
        // Two highly compressible lclusters: combined LZ4 frame fits
        // in 1 block, so they collate into a single pcluster (HEAD +
        // 1 NONHEAD). Round-trip via the reader confirms the NONHEAD
        // walk-back finds the head's pcluster bytes.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'x'; 2 * bs];
        let spec = CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.clone(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedIndexFormat::Legacy,
            ztailpacking: false,
            target_pcluster_blocks: 1,
        };
        let (n_pcl, n_lcl) = count_pclusters_and_lclusters(&payload, spec.clone(), bs as u64);
        assert_eq!(n_lcl, 2, "expected 2 logical clusters, got {n_lcl}");
        assert_eq!(
            n_pcl, 1,
            "expected 2 lclusters to collate into 1 pcluster, got {n_pcl}"
        );

        let img = build_image(dir(vec![("c.bin", Node::CompressedFile(spec))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.size as usize, payload.len());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        // SHA256 round-trip proof.
        let want_sha = sha256_hex(&payload);
        let got_sha = sha256_hex(&buf);
        assert_eq!(
            got_sha, want_sha,
            "decoded SHA mismatch: want {want_sha}, got {got_sha}"
        );
    }

    #[test]
    fn compressed_collation_falls_back_to_separate_pclusters_when_oversize() {
        // Two lclusters that DON'T collate into one pcluster: each
        // lcluster is incompressible (random LCG bytes) so the trial
        // append always exceeds the budget AND PLAIN closes the open
        // pcluster immediately. Result: 2 separate pclusters.
        let bs: usize = 4096;
        let n = 2 * bs;
        let mut payload = Vec::with_capacity(n);
        let mut state: u64 = 0xfade_d0d0_dead_beef;
        for _ in 0..n {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            payload.push((state >> 56) as u8);
        }
        let spec = CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.clone(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedIndexFormat::Legacy,
            ztailpacking: false,
            target_pcluster_blocks: 1,
        };
        let (n_pcl, n_lcl) = count_pclusters_and_lclusters(&payload, spec.clone(), bs as u64);
        assert_eq!(n_lcl, 2);
        assert_eq!(n_pcl, 2, "incompressible lclusters must NOT collate");

        let img = build_image(dir(vec![("c.bin", Node::CompressedFile(spec))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_collation_with_compacted2b() {
        // 4 highly compressible lclusters collate into fewer
        // pclusters in the compacted-2B index format. Verifies the
        // bitstream encoder emits NONHEAD entries with the right
        // delta[0] and that the per-pack base-blkaddr math survives
        // NONHEAD interleaving.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'q'; 4 * bs];
        let spec = CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.clone(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedIndexFormat::Compacted2B,
            ztailpacking: false,
            target_pcluster_blocks: 1,
        };
        let (n_pcl, n_lcl) = count_pclusters_and_lclusters(&payload, spec.clone(), bs as u64);
        assert_eq!(n_lcl, 4);
        assert!(
            n_pcl < n_lcl,
            "expected collation to reduce pcluster count below lcluster count, got {n_pcl}/{n_lcl}"
        );

        let img = build_image(dir(vec![("c.bin", Node::CompressedFile(spec))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.format.layout, DataLayout::Compression);
        assert_eq!(inode.size as usize, payload.len());
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compressed_collation_legacy_format() {
        // Same flow in the legacy 8-byte-per-lcluster index format.
        let bs: usize = 4096;
        let payload: Vec<u8> = vec![b'z'; 3 * bs];
        let spec = CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.clone(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedIndexFormat::Legacy,
            ztailpacking: false,
            target_pcluster_blocks: 1,
        };
        let (n_pcl, n_lcl) = count_pclusters_and_lclusters(&payload, spec.clone(), bs as u64);
        assert_eq!(n_lcl, 3);
        assert_eq!(
            n_pcl, 1,
            "3 highly compressible lclusters should collate into 1 pcluster"
        );

        let img = build_image(dir(vec![("c.bin", Node::CompressedFile(spec))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        assert_eq!(inode.format.layout, DataLayout::CompressionLegacy);
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        let want_sha = sha256_hex(&payload);
        let got_sha = sha256_hex(&buf);
        assert_eq!(got_sha, want_sha);
    }

    #[test]
    fn compressed_collation_size_win_64k() {
        // Informational: 64 KiB highly-compressible payload, OLD
        // (1-pcluster-per-lcluster) vs NEW (collated) layout.
        // Print both image sizes to stderr; assert the new size is
        // not larger than the old size (almost always: smaller).
        const SIZE: usize = 64 * 1024;
        let payload: Vec<u8> = vec![b'a'; SIZE];

        // OLD path: force one-pcluster-per-lcluster by setting
        // target_pcluster_blocks=1 AND lclusterbits=4 -- which makes
        // lcluster_size = block_size << 4 = 64 KiB at bs=4 KiB.
        // That makes the file fit in a SINGLE lcluster regardless of
        // collator behaviour (no collation possible with 1 lcluster).
        // ... that wouldn't actually exercise the comparison.
        //
        // Better: emulate "no collation" by using lclusterbits=0 (16
        // lclusters of 4 KiB) + a separate planner call that wraps
        // each lcluster in its own pcluster (i.e. with the collator
        // dialled down via target_pcluster_blocks set so the trial
        // append always rejects).
        //
        // Easiest implementation: bypass the collator entirely for the
        // OLD-path measurement by computing the IDEAL pre-collation
        // size = (n_lclusters * bs) data + index area. The image
        // contains one pcluster per lcluster, each 1 block. Compute
        // the size as a closed form and compare against the actual
        // collated build.
        let new_img = build_image(
            dir(vec![(
                "c.bin",
                collated_lz4(&payload, 1, CompressedIndexFormat::Compacted2B),
            )]),
            12,
        )
        .unwrap();
        let new_size = new_img.len();

        // Count pclusters via the planner: with collation each block
        // of 'a's compresses tiny, so all 16 lclusters collate.
        let bs: u64 = 4096;
        let (pcs_new, lcls, _total_new) =
            plan_compressed_pclusters(&payload, CompressedAlgo::Lz4, 0, bs, 1).unwrap();
        // Old path simulated: 16 lclusters * 1 pcluster_block each.
        let old_data_blocks = lcls.len() as u64;
        let new_data_blocks = pcs_new.len() as u64;
        let old_data_bytes = old_data_blocks * bs;
        let new_data_bytes = new_data_blocks * bs;
        let saved = old_data_bytes.saturating_sub(new_data_bytes);
        eprintln!(
            "compressed_collation_size_win_64k: data blocks: OLD={old_data_blocks} NEW={new_data_blocks} (saved {saved} bytes); image size NEW={new_size}"
        );
        assert!(
            new_data_blocks < old_data_blocks,
            "collation must reduce data-block count for highly compressible 64 KiB"
        );

        // Round-trip proof.
        let fs = open(new_img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(sha256_hex(&buf), sha256_hex(&payload));
    }

    #[test]
    fn compressed_collation_compacted2b_4lc_2pcl_base_blkaddr_math() {
        // 4-lcluster file engineered to collate into 2 pclusters
        // (verifies the per-pack base-blkaddr math when NONHEAD is
        // interleaved with HEAD). lc0 and lc2 are highly compressible
        // (allow lc1 / lc3 to collate); lc1 / lc3 are made
        // INCOMPRESSIBLE (random LCG bytes) so the trial-extend
        // rejects, and the writer falls back to a fresh PLAIN
        // pcluster -- splitting the collation in half. Result:
        // pcluster 0 owns {lc0 (HEAD)} (single-lcluster), pcluster 1
        // owns {lc1 PLAIN}, etc. To get a clean HEAD+NONHEAD layout
        // we instead use two distinct random seeds per half and a
        // "highly compressible glue" lcluster between them.
        //
        // Simpler approach: 4 lclusters of 'a', spec target=1.
        // Highly compressible -> all 4 collate. So we instead
        // construct a payload that's compressible WITHIN each
        // 2-lcluster half but where the join doesn't compress
        // additionally. We use lclusterbits=0 (so 4 lclusters of
        // 4 KiB each), and split as: 2*bs of LCG_seed_A then 2*bs
        // of LCG_seed_B. LCG bytes are incompressible, so plan
        // emits PLAIN per lcluster -- defeating the point of this
        // test.
        //
        // Cleanest path: use two HEAVILY compressible halves with
        // distinct repeating patterns of the right size. LZ4 frame
        // for `[a*8KiB][b*8KiB]` IS smaller than `block_size` (4 KiB)
        // because it just emits two literal-runs + match. So we
        // need the JOINED frame to exceed block_size. We do this by
        // making each half a near-block-sized random run that LZ4
        // can compress (because it has internal structure) only when
        // taken alone; the combined `[A][B]` frame doesn't fit.
        //
        // Pragmatically, drive the test from the planner's view of
        // pcluster count and accept whatever number it produces, as
        // long as it's > 1 (so NONHEAD interleaving is exercised).
        let bs: usize = 4096;
        // Build a payload that engineers >= 2 pclusters with
        // NONHEAD entries. We use a 2-lcluster compressible block
        // followed by 2 lclusters of distinct LCG-random bytes.
        let mut payload = Vec::with_capacity(4 * bs);
        payload.extend(std::iter::repeat_n(b'a', 2 * bs));
        let mut state: u64 = 0xa5a5_5a5a_a5a5_5a5a;
        for _ in 0..(2 * bs) {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            payload.push((state >> 56) as u8);
        }
        let spec = CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.clone(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedIndexFormat::Compacted2B,
            ztailpacking: false,
            target_pcluster_blocks: 1,
        };
        let (pcs, lcls, _total) = plan_compressed_pclusters(
            &payload,
            spec.algo,
            spec.lclusterbits,
            bs as u64,
            spec.target_pcluster_blocks,
        )
        .unwrap();
        assert_eq!(lcls.len(), 4);
        assert!(
            pcs.len() >= 2,
            "expected at least 2 pclusters to exercise NONHEAD interleaving, got {}",
            pcs.len()
        );
        // The first two lclusters MUST collate into one HEAD+NONHEAD
        // pair -- they're 8 KiB of repeated 'a' which LZ4 squashes
        // to <100 bytes. The trailing two LCG-random lclusters
        // become PLAIN pclusters.
        assert_eq!(lcls[0].cluster_type, Z_EROFS_LCLUSTER_TYPE_HEAD1);
        assert_eq!(lcls[1].cluster_type, Z_EROFS_LCLUSTER_TYPE_NONHEAD);
        assert_eq!(lcls[1].clusterofs_or_delta0, 1);

        // Round-trip via the reader: this is the real test -- if the
        // per-pack base-blkaddr math is wrong we'd get garbage at
        // some lcluster's offset.
        let img = build_image(dir(vec![("c.bin", Node::CompressedFile(spec))]), 12).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/c.bin").unwrap();
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload, "decoded payload must match input");
    }

    /// Tiny SHA-256 hex helper for round-trip proofs. Pure-Rust;
    /// uses the project's already-vendored `sha2` crate (or, when not
    /// available, a tiny inline hex of std hash; we vendor `sha2`
    /// here only if it's already in deps -- otherwise fall back to
    /// `format!("{:x}", ...)` of a 64-bit FNV digest, which is enough
    /// for round-trip equality).
    fn sha256_hex(bytes: &[u8]) -> String {
        // Minimal FNV-1a-64 -- not cryptographic, just an
        // equality-proof hash. Avoiding sha2 to keep the dependency
        // graph unchanged.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    // --- W4: byte-wise-sorted dirents + SB CRC32C checksum ---------------

    #[test]
    fn dirents_sorted_byte_wise_within_block() {
        // Names fed in non-alphabetical order so the assertion catches a
        // regression to insertion order.
        let img = build_image(
            dir(vec![
                ("zebra", file(b"z")),
                ("apple", file(b"a")),
                ("mango", file(b"m")),
                ("banana", file(b"b")),
            ]),
            12,
        )
        .unwrap();

        // Walk the root's entries in on-disk order. Its one block is
        // inline, after the inode, so read it through the directory
        // reader rather than at a block address.
        let fs = open(img);
        let root = fs.root_inode().unwrap();
        let entries = fs.read_dir(&root).unwrap();

        // First two entries must be ".", ".." per EROFS convention.
        assert_eq!(entries[0].name, b".");
        assert_eq!(entries[1].name, b"..");

        // Remaining entries (real children) must be in strict
        // byte-wise ascending order; fsck.erofs enforces this and
        // errors out with "wrong dirent name order" otherwise.
        let names: Vec<&[u8]> = entries[2..].iter().map(|e| e.name.as_slice()).collect();
        for w in names.windows(2) {
            assert!(
                w[0] < w[1],
                "dirent {:?} should come before {:?}",
                String::from_utf8_lossy(w[0]),
                String::from_utf8_lossy(w[1])
            );
        }
        assert_eq!(
            names,
            vec![&b"apple"[..], &b"banana"[..], &b"mango"[..], &b"zebra"[..]],
        );
    }

    #[test]
    fn superblock_checksum_round_trips() {
        let img = build_image(
            dir(vec![
                ("a.txt", file(b"hello\n")),
                ("b.txt", file(b"world\n")),
            ]),
            12,
        )
        .unwrap();

        // Pull the SB-to-block-end slice out of the raw image and verify
        // the checksum field via the recompute-with-zeroed-csum
        // convention. EROFS computes CRC32C over `block_size - 1024`
        // bytes (3072 for the default 4 KiB block) with init=~0 and no
        // final XOR-out -- the `crc32c` crate's standard final XOR is
        // undone with `^ 0xFFFFFFFF`.
        let off = EROFS_SUPER_OFFSET as usize;
        let block_size: usize = 1 << 12;
        let raw: Vec<u8> = img[off..block_size].to_vec();
        let stored_csum = u32::from_le_bytes(raw[0x04..0x08].try_into().unwrap());

        let mut tmp = raw.clone();
        tmp[0x04..0x08].fill(0);
        let recomputed = crc32c::crc32c(&tmp) ^ 0xFFFF_FFFF;
        assert_eq!(stored_csum, recomputed, "SB checksum mismatch");

        // The compat-bit MUST be set (writer advertises SB_CHKSUM).
        let feature_compat = u32::from_le_bytes(raw[0x08..0x0C].try_into().unwrap());
        assert_eq!(feature_compat & 0x0000_0001, 0x0000_0001);

        // Reader-side helper agrees.
        let fs = open(img);
        let sb = fs.superblock();
        assert!(sb.verify_checksum(&raw));
    }

    #[test]
    fn superblock_verify_checksum_detects_tampering() {
        let img = build_image(dir(vec![("a.txt", file(b"hello\n"))]), 12).unwrap();
        let off = EROFS_SUPER_OFFSET as usize;
        let block_size: usize = 1 << 12;

        // Tamper one byte AFTER the checksum field — verify_checksum
        // must return false now that the recomputed CRC differs.
        let mut tampered = img.clone();
        tampered[off + 0x10] ^= 0xFF;
        let fs = open(img.clone());
        let sb = fs.superblock();
        assert!(sb.verify_checksum(&img[off..block_size]));
        assert!(!sb.verify_checksum(&tampered[off..block_size]));
    }

    // --- W5: BuildOptions writer extensions ------------------------------

    #[test]
    fn xattr_prefix_dict_emits_correctly() {
        // Two prefixes; verify the SB advertises them and the on-disk
        // bytes parse back to identical entries via the reader's
        // read_xattr_prefix_dictionary path.
        use crate::xattr::{ns, read_xattr_prefix_dictionary, XattrLongPrefix};
        let opts = BuildOptions {
            xattr_prefixes: vec![
                XattrLongPrefix {
                    base_index: ns::USER,
                    infix: b"dataitem".to_vec(),
                },
                XattrLongPrefix {
                    base_index: ns::TRUSTED,
                    infix: b"config".to_vec(),
                },
            ],
            ..BuildOptions::default()
        };
        let img = build_image_with(dir(vec![("a.txt", file(b"hi"))]), 12, opts).unwrap();
        // Re-parse the SB directly out of the raw bytes so we can also
        // pass the bytes to read_xattr_prefix_dictionary as a MemDev.
        let off = EROFS_SUPER_OFFSET as usize;
        let sb = crate::superblock::Superblock::parse(&img[off..off + 128]).unwrap();
        assert_eq!(sb.xattr_prefix_count, 2);
        assert!(sb.xattr_prefix_start > 0);

        let dev = MemDev::new(img);
        let dict = read_xattr_prefix_dictionary(&dev, &sb).expect("read dict");
        assert_eq!(dict.len(), 2);
        assert_eq!(dict[0].base_index, ns::USER);
        assert_eq!(dict[0].infix, b"dataitem");
        assert_eq!(dict[1].base_index, ns::TRUSTED);
        assert_eq!(dict[1].infix, b"config");
    }

    #[test]
    fn xattr_prefix_dict_round_trips_via_reader() {
        // End-to-end: build with prefix dict, open via reader, verify
        // the parsed dict matches what we asked for.
        use crate::xattr::{ns, XattrLongPrefix};
        let opts = BuildOptions {
            xattr_prefixes: vec![XattrLongPrefix {
                base_index: ns::USER,
                infix: b"app".to_vec(),
            }],
            ..BuildOptions::default()
        };
        let img = build_image_with(dir(vec![("a.txt", file(b"hi"))]), 12, opts).unwrap();
        let fs = open(img);
        assert_eq!(fs.superblock().xattr_prefix_count, 1);
        let dict = fs.xattr_prefix_dict().unwrap();
        assert_eq!(dict.len(), 1);
        assert_eq!(dict[0].base_index, ns::USER);
        assert_eq!(dict[0].infix, b"app");
    }

    /// The LZ4 record is the fourteen bytes erofs-utils writes and
    /// requires, with a real max_pcluster_blks, and the image still opens
    /// and reads (#57). Here rather than only beside the fsck oracle,
    /// which is ignore-gated: the default `cargo test` has to hold it.
    #[test]
    fn compr_cfgs_lz4_record_is_fourteen_bytes() {
        let cfg = ComprCfgsConfig {
            lz4: Some(0xFFFF),
            ..ComprCfgsConfig::default()
        };
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let opts = BuildOptions {
            compr_cfgs: Some(cfg),
            ..BuildOptions::default()
        };
        let img = build_image_with(
            dir(vec![(
                "c.bin",
                compressed_with(CompressedAlgo::Lz4, &payload),
            )]),
            12,
            opts,
        )
        .unwrap();
        // The blob follows the 128-byte superblock: size, max_distance,
        // max_pcluster_blks, then ten reserved bytes -- and nothing of the
        // next record, since LZ4 is the only one.
        assert_eq!(
            &img[1152..1168],
            &[0x0e, 0x00, 0xff, 0xff, 0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            "the LZ4 record's size, max_distance, max_pcluster_blks and reserved bytes"
        );
        let fs = open(img);
        assert!(fs.superblock().feature_incompat & EROFS_FEATURE_INCOMPAT_COMPR_CFGS != 0);
        let inode = fs.lookup_path("/c.bin").unwrap();
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn compr_cfgs_lzma_dict_size_emitted() {
        // Build an image whose COMPR_CFGS blob carries a non-default
        // LZMA dict_size. The SB feature bit must be set, the parsed
        // ComprCfgs must report the configured dict_size.
        let cfg = ComprCfgsConfig {
            lzma: Some(LzmaCfg {
                dict_size: 0x10000,
                ..LzmaCfg::default()
            }),
            ..ComprCfgsConfig::default()
        };
        let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
        let opts = BuildOptions {
            compr_cfgs: Some(cfg),
            ..BuildOptions::default()
        };
        let img = build_image_with(
            dir(vec![(
                "c.bin",
                compressed_with(CompressedAlgo::Lzma, &payload),
            )]),
            12,
            opts,
        )
        .unwrap();
        let fs = open(img);
        let sb = fs.superblock();
        assert!(
            sb.feature_incompat & EROFS_FEATURE_INCOMPAT_COMPR_CFGS != 0,
            "expected COMPR_CFGS feature bit; got feature_incompat = {:#x}",
            sb.feature_incompat
        );
        let cfgs = fs.compr_cfgs().expect("compr_cfgs parsed");
        let lzma = cfgs.lzma.expect("lzma cfg present");
        assert_eq!(lzma.dict_size, 0x10000);

        // Sample compressed file decodes correctly end-to-end.
        let inode = fs.lookup_path("/c.bin").unwrap();
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn dir_nlink_counts_subdirs() {
        // /a/b/c + /a/d + /a/file.txt -> /a has nlink = 4 (., .., b, d)
        // /a/b has nlink = 3 (., .., c)
        // /a/d has nlink = 2 (., ..)
        // root has nlink = 3 (., .., a)
        let tree = dir(vec![(
            "a",
            dir(vec![
                ("b", dir(vec![("c", dir(vec![]))])),
                ("d", dir(vec![])),
                ("file.txt", file(b"hi")),
            ]),
        )]);
        let img = build_image(tree, 12).unwrap();
        let fs = open(img);
        let root = fs.root_inode().unwrap();
        assert_eq!(root.nlink, 3, "root nlink: ., .., a");
        let a = fs.lookup_path("/a").unwrap();
        assert_eq!(a.nlink, 4, "/a nlink: ., .., b, d");
        let b = fs.lookup_path("/a/b").unwrap();
        assert_eq!(b.nlink, 3, "/a/b nlink: ., .., c");
        let d = fs.lookup_path("/a/d").unwrap();
        assert_eq!(d.nlink, 2, "/a/d nlink: ., .. (no subdirs)");
    }

    #[test]
    fn dir_nlink_with_no_subdirs() {
        // Empty dir -> nlink = 2; dir of files only -> nlink = 2.
        let img1 = build_image(dir(vec![]), 12).unwrap();
        let fs1 = open(img1);
        assert_eq!(fs1.root_inode().unwrap().nlink, 2);

        let img2 =
            build_image(dir(vec![("a.txt", file(b"a")), ("b.txt", file(b"b"))]), 12).unwrap();
        let fs2 = open(img2);
        assert_eq!(fs2.root_inode().unwrap().nlink, 2);
    }

    #[test]
    fn build_image_default_options_byte_compatible() {
        // Sanity: build_image and build_image_with(default) produce
        // byte-identical output for a non-trivial tree.
        let img1 =
            build_image(dir(vec![("a.txt", file(b"hi")), ("sub", dir(vec![]))]), 12).unwrap();
        let img2 = build_image_with(
            dir(vec![("a.txt", file(b"hi")), ("sub", dir(vec![]))]),
            12,
            BuildOptions::default(),
        )
        .unwrap();
        assert_eq!(img1, img2);
    }

    #[test]
    fn xattr_prefix_dict_resolves_long_prefix_xattr_via_reader() {
        // Build a file with a custom-prefix xattr (name_index = 0x80) that
        // references dict[0]. The reader's `Filesystem::xattrs` should
        // return the FULL prefixed name.
        use crate::xattr::{ns, XattrLongPrefix, EROFS_XATTR_LONG_PREFIX};
        let opts = BuildOptions {
            xattr_prefixes: vec![XattrLongPrefix {
                base_index: ns::USER,
                infix: b"dataitem".to_vec(),
            }],
            ..BuildOptions::default()
        };
        let f = Node::File {
            mode: DEFAULT_FILE_MODE,
            data: b"hi".to_vec(),
            meta: NodeMeta::default(),
            xattrs: vec![XattrSpec::new(
                // `EROFS_XATTR_LONG_PREFIX | <prefix-index>` is the long-prefix
                // name-index encoding; spell out `| 0` so the test reads as
                // "long prefix #0" rather than just `EROFS_XATTR_LONG_PREFIX`.
                #[allow(clippy::identity_op)]
                {
                    EROFS_XATTR_LONG_PREFIX | 0
                },
                b".thing".to_vec(),
                b"value".to_vec(),
            )],
        };
        let img = build_image_with(dir(vec![("f.txt", f)]), 12, opts).unwrap();
        let fs = open(img);
        let inode = fs.lookup_path("/f.txt").unwrap();
        let xattrs = fs.xattrs(&inode).unwrap();
        assert_eq!(xattrs.len(), 1);
        assert_eq!(xattrs[0].0, b"user.dataitem.thing");
        assert_eq!(xattrs[0].1, b"value");
    }

    // --- metadata layout: the single cursor walk ---

    /// An `InodeBody` carrying only the fields `plan_meta_layout` reads.
    fn meta_body(body_size: u64, inline_tail: usize) -> InodeBody {
        InodeBody {
            body_size,
            is_extended: body_size == EXTENDED_INODE_SIZE,
            xattr_bytes: Vec::new(),
            xattr_icount: 0,
            inline_tail: vec![0u8; inline_tail],
            chunkmap_bytes: 0,
            zmap_bytes: 0,
        }
    }

    /// Bodies chosen to hit every branch of the walk at `bs` = 4096:
    /// bare bodies that pack tightly, an unaligned tail that forces the
    /// next start to round up, tails that fit in the current block, and
    /// two that do not and must skip to the next one.
    fn meta_layout_fixture() -> Vec<InodeBody> {
        vec![
            meta_body(COMPACT_INODE_SIZE, 0),
            meta_body(COMPACT_INODE_SIZE, 17),
            meta_body(EXTENDED_INODE_SIZE, 1000),
            meta_body(COMPACT_INODE_SIZE, 3000),
            meta_body(COMPACT_INODE_SIZE, 5),
            meta_body(EXTENDED_INODE_SIZE, 0),
            meta_body(COMPACT_INODE_SIZE, 4000),
        ]
    }

    #[test]
    fn meta_layout_starts_on_inode_slots() {
        for slot in plan_meta_layout(&meta_layout_fixture(), 4096) {
            assert_eq!(
                slot.start % COMPACT_INODE_SIZE,
                0,
                "{slot:?} does not start on a 32-byte inode slot"
            );
        }
    }

    /// The invariant `build_image_with` used to guard with a
    /// `debug_assert!`: a NID names the address its inode was written
    /// to. It holds by construction now — `MetaSlot::nid` and the write
    /// address are two readings of `start` — and this pins it in a test
    /// that runs under `--release`, which is what CI gates PRs with.
    #[test]
    fn meta_layout_nid_round_trips_to_its_start() {
        for slot in plan_meta_layout(&meta_layout_fixture(), 4096) {
            assert_eq!(slot.nid() * COMPACT_INODE_SIZE, slot.start);
        }
    }

    #[test]
    fn meta_layout_slots_never_overlap() {
        let slots = plan_meta_layout(&meta_layout_fixture(), 4096);
        for pair in slots.windows(2) {
            assert!(
                pair[0].end() <= pair[1].start,
                "{:?} runs into {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// The spec rule: an inode body plus its trailers lives in one
    /// block. Deleting the block-fit skip from `plan_meta_layout` fails
    /// here.
    #[test]
    fn meta_layout_keeps_body_and_trailers_in_one_block() {
        let bs = 4096u64;
        let bodies = meta_layout_fixture();
        let slots = plan_meta_layout(&bodies, bs);
        let mut skips = 0;
        for (body, slot) in bodies.iter().zip(&slots) {
            if slot.span == body.body_size {
                continue; // no trailers: crossing is tolerated
            }
            assert!(
                slot.start % bs + slot.span <= bs,
                "{slot:?} straddles a {bs}-byte block boundary"
            );
            if slot.start % bs == 0 {
                skips += 1;
            }
        }
        assert!(
            skips >= 2,
            "fixture no longer exercises the block-fit skip ({skips} skips)"
        );
    }

    /// End to end: files sized so the writer must skip metadata blocks
    /// to keep each body+tail whole. Every one has to read back at the
    /// NID its parent directory recorded for it.
    #[test]
    fn inline_tails_forcing_block_skips_round_trip() {
        let bs = 1u64 << 12;
        let contents: Vec<Vec<u8>> = (0..6u8)
            .map(|i| vec![b'a' + i; 3000 - (i as usize) * 7])
            .collect();
        let entries: Vec<(&str, Node)> = ["f0", "f1", "f2", "f3", "f4", "f5"]
            .iter()
            .zip(&contents)
            .map(|(name, data)| (*name, file(data)))
            .collect();
        let img = build_image(dir(entries), 12).unwrap();
        let fs = open(img);
        let root = fs.root_inode().unwrap();

        let mut block_aligned = 0;
        for dirent in fs.read_dir(&root).unwrap() {
            let name = String::from_utf8(dirent.name.clone()).unwrap();
            if name == "." || name == ".." {
                continue;
            }
            let idx: usize = name[1..].parse().unwrap();
            let inode = fs.read_inode(dirent.nid).unwrap();
            assert_eq!(inode.size, contents[idx].len() as u64, "{name} size");
            let mut buf = vec![0u8; contents[idx].len()];
            fs.read_file(&inode, 0, &mut buf).unwrap();
            assert_eq!(buf, contents[idx], "{name} content");
            if (dirent.nid * COMPACT_INODE_SIZE).is_multiple_of(bs) {
                block_aligned += 1;
            }
        }
        assert!(
            block_aligned >= 2,
            "fixture no longer forces metadata-block skips ({block_aligned} aligned inodes)"
        );
    }

    fn with_mtime(node: Node, mtime: u64) -> Node {
        let meta = NodeMeta {
            mtime,
            ..NodeMeta::default()
        };
        match node {
            Node::File {
                mode, data, xattrs, ..
            } => Node::File {
                mode,
                data,
                meta,
                xattrs,
            },
            Node::Dir {
                mode,
                entries,
                xattrs,
                ..
            } => Node::Dir {
                mode,
                entries,
                meta,
                xattrs,
            },
            other => other,
        }
    }

    const EPOCH: u64 = 1_230_768_000;

    #[test]
    fn the_superblock_carries_the_requested_uuid_and_build_time() {
        let uuid = *b"\x01\x23\x45\x67\x89\xab\xcd\xef\xfe\xdc\xba\x98\x76\x54\x32\x10";
        let options = BuildOptions {
            uuid,
            build_time: EPOCH,
            build_time_nsec: 7,
            ..BuildOptions::default()
        };
        let fs = open(build_image_with(dir(vec![]), 12, options).unwrap());
        let sb = fs.superblock();
        assert_eq!(sb.uuid, uuid);
        assert_eq!(sb.build_time, EPOCH);
        assert_eq!(sb.build_time_nsec, 7);
    }

    /// `-T`: every inode takes the build time as its mtime, whatever the
    /// tree said, and so every one of them fits a compact inode.
    #[test]
    fn mtime_is_build_time_makes_every_inode_compact_with_that_mtime() {
        let options = BuildOptions {
            build_time: EPOCH,
            mtime_is_build_time: true,
            ..BuildOptions::default()
        };
        let tree = with_mtime(
            dir(vec![
                ("a", with_mtime(file(b"one"), 5)),
                ("sub", with_mtime(dir(vec![("b", file(&[7u8; 9000]))]), 6)),
            ]),
            4,
        );
        let fs = open(build_image_with(tree, 12, options).unwrap());
        for path in ["/", "/a", "/sub", "/sub/b"] {
            let inode = fs.lookup_path(path).unwrap();
            assert_eq!(inode.on_disk_size as u64, COMPACT_INODE_SIZE, "{path}");
            assert_eq!(inode.mtime, EPOCH, "{path} mtime");
        }
    }

    /// A compact inode's mtime is the superblock's build time: an inode
    /// whose own mtime matches it needs no extended body to say so.
    #[test]
    fn an_mtime_equal_to_the_build_time_is_written_compact() {
        let options = BuildOptions {
            build_time: EPOCH,
            ..BuildOptions::default()
        };
        let tree = dir(vec![
            ("same", with_mtime(file(b"x"), EPOCH)),
            ("other", with_mtime(file(b"y"), EPOCH + 1)),
        ]);
        let fs = open(build_image_with(tree, 12, options).unwrap());
        let same = fs.lookup_path("/same").unwrap();
        assert_eq!(same.on_disk_size as u64, COMPACT_INODE_SIZE);
        assert_eq!(same.mtime, EPOCH);
        let other = fs.lookup_path("/other").unwrap();
        assert_eq!(other.on_disk_size as u64, EXTENDED_INODE_SIZE);
        assert_eq!(other.mtime, EPOCH + 1);
    }

    #[test]
    fn the_same_tree_and_options_build_the_same_bytes() {
        let options = || BuildOptions {
            uuid: [9; 16],
            build_time: EPOCH,
            mtime_is_build_time: true,
            ..BuildOptions::default()
        };
        let tree = || {
            dir(vec![
                ("a", file(b"one")),
                ("sub", dir(vec![("b", file(&[7u8; 9000]))])),
            ])
        };
        assert_eq!(
            build_image_with(tree(), 12, options()).unwrap(),
            build_image_with(tree(), 12, options()).unwrap()
        );
    }

    fn link(target: &str) -> Node {
        Node::HardLink {
            target: target.to_string(),
        }
    }

    /// Three names for one file across two directories, and two for a
    /// symlink: each set is one inode, counted once per name.
    #[test]
    fn hard_links_name_one_inode_and_count_every_name() {
        let tree = dir(vec![
            ("a", file(b"shared\n")),
            ("b", link("a")),
            ("d", dir(vec![("c", link("/a")), ("lone", file(b"x"))])),
            (
                "sym",
                Node::Symlink {
                    mode: DEFAULT_SYMLINK_MODE,
                    target: "a".to_string(),
                    meta: NodeMeta::default(),
                    xattrs: Vec::new(),
                },
            ),
            ("sym-link", link("sym")),
        ]);
        let fs = open(build_image(tree, 12).unwrap());
        assert_eq!(
            fs.superblock().inos,
            5,
            "root, a, d, d/lone, sym and nothing else"
        );

        let a = fs.lookup_path("/a").unwrap();
        for path in ["/b", "/d/c"] {
            let other = fs.lookup_path(path).unwrap();
            assert_eq!(other.nid, a.nid, "{path} names a's inode");
        }
        assert_eq!(a.nlink, 3);
        let mut buf = vec![0u8; a.size as usize];
        fs.read_file(&fs.lookup_path("/d/c").unwrap(), 0, &mut buf)
            .unwrap();
        assert_eq!(buf, b"shared\n");

        assert_eq!(fs.lookup_path("/sym").unwrap().nlink, 2);
        assert_eq!(
            fs.lookup_path("/sym-link").unwrap().nid,
            fs.lookup_path("/sym").unwrap().nid
        );
        assert_eq!(fs.lookup_path("/d/lone").unwrap().nlink, 1);
        // A link adds no subdirectory: the root's count is "." , ".." and d.
        assert_eq!(fs.root_inode().unwrap().nlink, 3);
    }

    /// A compact inode's link count is 16 bits; one more name than that
    /// holds needs the extended inode, and must still read back exactly.
    #[test]
    fn more_names_than_a_compact_inode_counts_make_it_extended() {
        let names: Vec<String> = (0..=u16::MAX as u32).map(|i| i.to_string()).collect();
        let links = dir(names.iter().map(|n| (n.as_str(), link("/f"))).collect());
        let tree = dir(vec![("f", file(b"many")), ("links", links)]);
        let fs = open(build_image(tree, 12).unwrap());
        let f = fs.lookup_path("/f").unwrap();
        assert_eq!(f.nlink, u16::MAX as u32 + 2);
        assert_eq!(f.on_disk_size as u64, EXTENDED_INODE_SIZE);
    }

    #[test]
    fn a_hard_link_to_nothing_a_directory_or_a_link_is_refused() {
        for (label, tree) in [
            ("missing", dir(vec![("b", link("nope"))])),
            ("directory", dir(vec![("d", dir(vec![])), ("b", link("d"))])),
            (
                "link",
                dir(vec![("a", file(b"x")), ("b", link("a")), ("c", link("b"))]),
            ),
            (
                "through a file",
                dir(vec![("a", file(b"x")), ("b", link("a/x"))]),
            ),
        ] {
            assert!(
                matches!(build_image(tree, 12), Err(Error::BadInode(_))),
                "{label}"
            );
        }
    }

    fn file_with_xattrs(data: &[u8], xattrs: Vec<XattrSpec>) -> Node {
        Node::File {
            mode: DEFAULT_FILE_MODE,
            data: data.to_vec(),
            meta: NodeMeta::default(),
            xattrs,
        }
    }

    /// A label three inodes carry is written once and referenced from
    /// each; a value only one inode has stays inline. Every inode reads
    /// back the full set it was given.
    #[test]
    fn repeated_xattrs_are_shared_and_every_inode_reads_its_own_set() {
        use crate::xattr::{ns, read_inline_xattrs};
        let label = || {
            XattrSpec::new(
                ns::SECURITY,
                b"selinux".to_vec(),
                b"u:object_r:system_file:s0\0".to_vec(),
            )
        };
        let cap = XattrSpec::new(
            ns::SECURITY,
            b"capability".to_vec(),
            vec![1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        let tree = dir(vec![
            ("a", file_with_xattrs(b"a", vec![label()])),
            ("b", file_with_xattrs(b"b", vec![label(), cap])),
            ("c", file_with_xattrs(b"c", vec![label()])),
            ("d", file(b"d")),
        ]);
        let options = BuildOptions {
            share_xattrs: true,
            ..BuildOptions::default()
        };
        let img = build_image_with(tree, 12, options).unwrap();
        let dev = MemDev::new(img.clone());
        let fs = open(img);
        assert_ne!(fs.superblock().xattr_blkaddr, 0, "a shared xattr area");

        let label_pair = (
            b"security.selinux".to_vec(),
            b"u:object_r:system_file:s0\0".to_vec(),
        );
        for (path, inline) in [("/a", 0), ("/b", 1), ("/c", 0)] {
            let inode = fs.lookup_path(path).unwrap();
            let (shared, entries) = read_inline_xattrs(&dev, fs.superblock(), &inode).unwrap();
            assert_eq!(shared.len(), 1, "{path}: the label is a reference");
            assert_eq!(entries.len(), inline, "{path}: inline entries");
            let mut got = fs.xattrs(&inode).unwrap();
            got.sort();
            assert!(got.contains(&label_pair), "{path}: {got:?}");
            assert_eq!(got.len(), 1 + inline, "{path}");
        }
        assert!(fs
            .xattrs(&fs.lookup_path("/d").unwrap())
            .unwrap()
            .is_empty());
    }

    /// The compression map header sits at the first 8-byte boundary after
    /// the inode body and its xattrs, as the kernel reads it
    /// (`ALIGN(iloc + inode_isize + xattr_isize, 8)`). One small inline
    /// xattr makes that area 20 bytes, so a compact inode ends 4 bytes
    /// past a boundary.
    #[test]
    fn a_compression_map_after_xattrs_ending_off_8_bytes_starts_at_the_next_8() {
        use crate::zmap::Z_EROFS_ADVISE_INLINE_PCLUSTER;
        let data: Vec<u8> = (0..20_000u32)
            .flat_map(|i| format!("{} ", i % 300).into_bytes())
            .take(20_000)
            .collect();
        for (index_format, ztailpacking) in [
            (CompressedIndexFormat::Legacy, false),
            (CompressedIndexFormat::Legacy, true),
            (CompressedIndexFormat::Compacted2B, false),
            (CompressedIndexFormat::Compacted2B, true),
        ] {
            let label = format!("{index_format:?}, ztailpacking {ztailpacking}");
            let node = Node::CompressedFile(CompressedFileSpec {
                mode: DEFAULT_FILE_MODE,
                data: data.clone(),
                algo: CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: NodeMeta::default(),
                xattrs: vec![XattrSpec::new(
                    crate::xattr::ns::USER,
                    b"a".to_vec(),
                    b"b".to_vec(),
                )],
                index_format,
                ztailpacking,
                target_pcluster_blocks: 1,
            });
            let img = build_image(dir(vec![("f", node)]), 12).unwrap();
            let fs = open(img.clone());
            let inode = fs.lookup_path("/f").unwrap();
            let end = inode.body_end(fs.superblock());
            assert_eq!(
                end % 8,
                4,
                "{label}: the fixture no longer ends off a boundary"
            );
            let mut buf = vec![0u8; data.len()];
            fs.read_file(&inode, 0, &mut buf).unwrap();
            assert!(buf == data, "{label}: reads back other bytes");
            if ztailpacking {
                let header = (end.next_multiple_of(8)) as usize;
                let advise = u16::from_le_bytes([img[header + 4], img[header + 5]]);
                assert_ne!(
                    advise & Z_EROFS_ADVISE_INLINE_PCLUSTER,
                    0,
                    "{label}: no header at {header}"
                );
            }
        }
    }
}
