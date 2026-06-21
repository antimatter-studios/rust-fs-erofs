//! Pure-Rust EROFS (Enhanced Read-Only File System) reader.
//!
//! **Phase 0 scope** — uncompressed images only. Reads superblock,
//! both compact and extended inode shapes, and FLAT_PLAIN /
//! FLAT_INLINE data layouts. Compressed (LZ4 / LZMA / DEFLATE) and
//! chunk-based inodes return `Error::UnsupportedLayout`.
//!
//! Generate a test image with `mkfs.erofs` (no `-z` flag = no
//! compression):
//!
//! ```sh
//! mkfs.erofs out.img source-tree/
//! ```
//!
//! Open it via [`Filesystem::open`] over any [`fs_core::BlockRead`] —
//! `FileDevice` for a path, `SliceReader` for an in-memory buffer.
//!
//! Spec: `linux/fs/erofs/erofs_fs.h` and `Documentation/filesystems/
//! erofs.rst`. Field names mirror the kernel struct names with the
//! `i_` / `s_` prefixes dropped where redundant.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod acl;
pub mod chunked;
pub mod decompress;
pub mod dir;
pub mod error;
pub mod fs;
pub mod inode;
pub mod layout;
pub mod mkfs;
pub mod superblock;
pub mod xattr;
pub mod zmap;

// C ABI exports — surface defined in `include/fs_erofs.h`.
pub mod capi;

pub use acl::{AclEntry, AclPerm, AclTag};
pub use chunked::{ChunkInfo, EROFS_NULL_ADDR};
pub use decompress::{decompress, decompress_with_config, Algorithm};
pub use dir::{DirEntry, EROFS_DIRENT_SIZE};
pub use error::{Error, Result};
pub use fs::Filesystem;
pub use inode::{FileType, Inode};
pub use layout::{DataLayout, InodeFormat, InodeVersion};
pub use superblock::{
    read_compr_cfgs, ComprCfgs, LzmaCfg, Superblock, EROFS_FEATURE_COMPAT_SB_CHKSUM,
    EROFS_FEATURE_INCOMPAT_COMPR_CFGS, EROFS_SUPER_MAGIC_V1, EROFS_SUPER_OFFSET,
};
pub use xattr::{
    parse_inline_xattrs, read_all_xattrs, read_inline_xattrs, read_shared_xattrs,
    read_xattr_prefix_dictionary, resolve_full_name, resolve_with_dict, XattrEntry,
    XattrLongPrefix, EROFS_XATTR_LONG_PREFIX, EROFS_XATTR_LONG_PREFIX_MASK,
};
pub use zmap::{ClusterMapping, ZMap};
