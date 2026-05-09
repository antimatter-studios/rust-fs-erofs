//! EROFS inode parsing.
//!
//! Two on-disk shapes coexist; `i_format`'s low bit picks between them:
//!
//! - **compact** (32 bytes, `erofs_inode_compact`): u16 size, u16 uid/gid,
//!   no mtime, single u32 i_u union.
//! - **extended** (64 bytes, `erofs_inode_extended`): u64 size, u32 uid/gid,
//!   u64 mtime + u32 nsec, u32 nlink.
//!
//! Both share the leading 4 bytes (`i_format`, `i_xattr_icount`), and
//! crucially share the i_u union at offset 0x10.
//!
//! NID-to-byte: an inode at NID `n` lives at
//! `meta_blkaddr * blocksize + n * 32`. The 32-byte stride is fixed even
//! for extended inodes (extended just consumes two consecutive 32-byte
//! slots). Source: `linux/fs/erofs/internal.h::erofs_iloc()`.

use crate::error::{Error, Result};
use crate::layout::{InodeFormat, InodeVersion};
use crate::superblock::Superblock;
use fs_core::BlockRead;

pub const EROFS_INODE_SLOT_SIZE: u64 = 32;

/// Linux POSIX `S_IF*` mode-type bits. Carried locally so we don't need
/// libc as a dep. Source: `linux/include/uapi/linux/stat.h`.
pub const S_IFMT: u16 = 0xF000;
pub const S_IFIFO: u16 = 0x1000;
pub const S_IFCHR: u16 = 0x2000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFBLK: u16 = 0x6000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFLNK: u16 = 0xA000;
pub const S_IFSOCK: u16 = 0xC000;

/// Inode file-type discriminator.
///
/// Hardlinks are not represented here -- in EROFS (and all Unix-like FS),
/// hardlinks are simply multiple dirents pointing at the same NID, so a
/// hardlinked file appears as `RegularFile` (or whatever its underlying
/// type is). The reader handles this transparently: each dirent lookup
/// resolves to the same `Inode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Dir,
    RegularFile,
    Symlink,
    ChrDev,
    BlkDev,
    Fifo,
    Sock,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Inode {
    /// NID this inode was loaded from. Used to compute the on-disk
    /// offset of inline data (which immediately follows the inode body
    /// + xattrs in the metadata area).
    pub nid: u64,
    pub format: InodeFormat,
    pub xattr_icount: u16,
    pub mode: u16,
    pub size: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub mtime_nsec: u32,
    pub ino: u32,
    /// Raw bytes 0x10..0x14 of the inode -- the i_u union. For
    /// FLAT_PLAIN / FLAT_INLINE this is `raw_blkaddr` (u32 LE). For
    /// chunked / compressed it carries other meanings.
    pub raw_u: u32,
    /// 32 (compact) or 64 (extended). Used by inline-data layouts to
    /// know where the body ends and the tail block begins.
    pub on_disk_size: u8,
}

impl Inode {
    /// Parse from a buffer beginning at the inode's first byte. The
    /// buffer must hold at least `on_disk_size` bytes (32 or 64).
    pub fn parse(nid: u64, bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 32 {
            return Err(Error::BadInode("buffer shorter than 32 bytes"));
        }
        let raw_format = u16::from_le_bytes(bytes[0x00..0x02].try_into().unwrap());
        let format = InodeFormat::parse(raw_format)?;
        let xattr_icount = u16::from_le_bytes(bytes[0x02..0x04].try_into().unwrap());
        let mode = u16::from_le_bytes(bytes[0x04..0x06].try_into().unwrap());
        let raw_u = u32::from_le_bytes(bytes[0x10..0x14].try_into().unwrap());

        match format.version {
            InodeVersion::Compact => {
                // size at 0x08 (u32), nlink at 0x06 (u16), uid at 0x18 (u16),
                // gid at 0x1A (u16), no mtime fields.
                let size = u32::from_le_bytes(bytes[0x08..0x0C].try_into().unwrap()) as u64;
                let nlink = u16::from_le_bytes(bytes[0x06..0x08].try_into().unwrap()) as u32;
                let ino = u32::from_le_bytes(bytes[0x14..0x18].try_into().unwrap());
                let uid = u16::from_le_bytes(bytes[0x18..0x1A].try_into().unwrap()) as u32;
                let gid = u16::from_le_bytes(bytes[0x1A..0x1C].try_into().unwrap()) as u32;
                Ok(Inode {
                    nid,
                    format,
                    xattr_icount,
                    mode,
                    size,
                    nlink,
                    uid,
                    gid,
                    mtime: 0,
                    mtime_nsec: 0,
                    ino,
                    raw_u,
                    on_disk_size: 32,
                })
            }
            InodeVersion::Extended => {
                if bytes.len() < 64 {
                    return Err(Error::BadInode("extended inode buffer < 64 bytes"));
                }
                // size at 0x08 (u64), uid at 0x18 (u32), gid at 0x1C (u32),
                // mtime at 0x20 (u64), mtime_nsec 0x28 (u32), nlink 0x2C (u32).
                let size = u64::from_le_bytes(bytes[0x08..0x10].try_into().unwrap());
                let ino = u32::from_le_bytes(bytes[0x14..0x18].try_into().unwrap());
                let uid = u32::from_le_bytes(bytes[0x18..0x1C].try_into().unwrap());
                let gid = u32::from_le_bytes(bytes[0x1C..0x20].try_into().unwrap());
                let mtime = u64::from_le_bytes(bytes[0x20..0x28].try_into().unwrap());
                let mtime_nsec = u32::from_le_bytes(bytes[0x28..0x2C].try_into().unwrap());
                let nlink = u32::from_le_bytes(bytes[0x2C..0x30].try_into().unwrap());
                Ok(Inode {
                    nid,
                    format,
                    xattr_icount,
                    mode,
                    size,
                    nlink,
                    uid,
                    gid,
                    mtime,
                    mtime_nsec,
                    ino,
                    raw_u,
                    on_disk_size: 64,
                })
            }
        }
    }

    /// On-disk byte offset of this inode's first byte.
    pub fn iloc(sb: &Superblock, nid: u64) -> u64 {
        sb.meta_blkaddr as u64 * sb.block_size() + nid * EROFS_INODE_SLOT_SIZE
    }

    /// Offset of the byte that immediately follows the inode body and
    /// any inline xattrs. For FLAT_INLINE this is where the tail block
    /// data starts. xattr layout: 12 bytes header + 4 bytes per icount
    /// slot. Source: `erofs_xattr_ibody_size()` in
    /// `linux/fs/erofs/xattr.h`.
    pub fn body_end(&self, sb: &Superblock) -> u64 {
        let inode_off = Inode::iloc(sb, self.nid);
        let xattr_size = if self.xattr_icount == 0 {
            0
        } else {
            // sizeof(erofs_xattr_ibody_header) + (icount - 1) * 4
            12 + (self.xattr_icount as u64 - 1) * 4
        };
        inode_off + self.on_disk_size as u64 + xattr_size
    }

    /// Read this inode by NID.
    pub fn read<R: BlockRead + ?Sized>(dev: &R, sb: &Superblock, nid: u64) -> Result<Self> {
        let off = Inode::iloc(sb, nid);
        let mut buf = [0u8; 64];
        dev.read_at(off, &mut buf)?;
        Inode::parse(nid, &buf)
    }

    pub fn is_dir(&self) -> bool {
        (self.mode & S_IFMT) == S_IFDIR
    }

    pub fn is_regular_file(&self) -> bool {
        (self.mode & S_IFMT) == S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        (self.mode & S_IFMT) == S_IFLNK
    }

    pub fn is_chrdev(&self) -> bool {
        (self.mode & S_IFMT) == S_IFCHR
    }

    pub fn is_blkdev(&self) -> bool {
        (self.mode & S_IFMT) == S_IFBLK
    }

    pub fn is_fifo(&self) -> bool {
        (self.mode & S_IFMT) == S_IFIFO
    }

    pub fn is_sock(&self) -> bool {
        (self.mode & S_IFMT) == S_IFSOCK
    }

    /// Classify the inode by mode-type bits.
    pub fn file_type(&self) -> FileType {
        match self.mode & S_IFMT {
            S_IFDIR => FileType::Dir,
            S_IFREG => FileType::RegularFile,
            S_IFLNK => FileType::Symlink,
            S_IFCHR => FileType::ChrDev,
            S_IFBLK => FileType::BlkDev,
            S_IFIFO => FileType::Fifo,
            S_IFSOCK => FileType::Sock,
            _ => FileType::Unknown,
        }
    }

    /// For chrdev/blkdev inodes, decode `i_u.rdev` into `(major, minor)`.
    /// Returns `None` for any other file type.
    ///
    /// Encoding: Linux's "new" 32-bit `dev_t` layout
    /// (`linux/include/uapi/linux/kdev_t.h`):
    ///
    /// ```text
    ///   major = (rdev >> 8) & 0xFFF
    ///   minor = (rdev & 0xFF) | ((rdev >> 12) & 0xFFF00)
    /// ```
    ///
    /// This subsumes the legacy 16-bit `(major << 8) | minor` form for
    /// any device with `major < 0x1000` and `minor < 0x100`, so a device
    /// like `sda2` (major=8, minor=2) yields `rdev = 0x0802` either way.
    pub fn rdev(&self) -> Option<(u32, u32)> {
        if !(self.is_chrdev() || self.is_blkdev()) {
            return None;
        }
        let r = self.raw_u;
        let major = (r >> 8) & 0xFFF;
        let minor = (r & 0xFF) | ((r >> 12) & 0xFFF00);
        Some((major, minor))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::DataLayout;

    /// Build a synthetic compact inode buffer.
    pub(crate) fn synth_compact(
        layout: DataLayout,
        mode: u16,
        size: u32,
        raw_blkaddr: u32,
    ) -> [u8; 32] {
        let mut b = [0u8; 32];
        // version=0 (compact), layout = layout bits at position 1..3
        let raw_format: u16 = (layout as u16) << 1;
        b[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        b[0x04..0x06].copy_from_slice(&mode.to_le_bytes());
        b[0x06..0x08].copy_from_slice(&1u16.to_le_bytes()); // nlink
        b[0x08..0x0C].copy_from_slice(&size.to_le_bytes());
        b[0x10..0x14].copy_from_slice(&raw_blkaddr.to_le_bytes());
        b
    }

    #[test]
    fn parse_compact_dir() {
        let buf = synth_compact(DataLayout::FlatPlain, 0x41ED, 4096, 5);
        let inode = Inode::parse(36, &buf).unwrap();
        assert_eq!(inode.on_disk_size, 32);
        assert_eq!(inode.size, 4096);
        assert_eq!(inode.raw_u, 5);
        assert!(inode.is_dir());
        assert!(!inode.is_regular_file());
    }

    #[test]
    fn parse_extended_file() {
        let mut b = [0u8; 64];
        let raw_format: u16 = 1 | ((DataLayout::FlatPlain as u16) << 1);
        b[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        b[0x04..0x06].copy_from_slice(&0x81A4u16.to_le_bytes()); // file, 0644
        b[0x08..0x10].copy_from_slice(&(1u64 << 40).to_le_bytes()); // 1 TiB
        b[0x10..0x14].copy_from_slice(&7u32.to_le_bytes());
        b[0x2C..0x30].copy_from_slice(&3u32.to_le_bytes());
        let inode = Inode::parse(99, &b).unwrap();
        assert_eq!(inode.on_disk_size, 64);
        assert_eq!(inode.size, 1u64 << 40);
        assert_eq!(inode.nlink, 3);
        assert!(inode.is_regular_file());
    }

    #[test]
    fn predicates_cover_all_file_types() {
        let cases: &[(u16, FileType)] = &[
            (S_IFDIR | 0o755, FileType::Dir),
            (S_IFREG | 0o644, FileType::RegularFile),
            (S_IFLNK | 0o777, FileType::Symlink),
            (S_IFCHR | 0o600, FileType::ChrDev),
            (S_IFBLK | 0o660, FileType::BlkDev),
            (S_IFIFO | 0o644, FileType::Fifo),
            (S_IFSOCK | 0o755, FileType::Sock),
            (0o644, FileType::Unknown),
        ];
        for (mode, expected) in cases {
            let buf = synth_compact(DataLayout::FlatPlain, *mode, 0, 0);
            let inode = Inode::parse(0, &buf).unwrap();
            assert_eq!(inode.file_type(), *expected, "mode=0x{:04x}", mode);
            assert_eq!(inode.is_dir(), *expected == FileType::Dir);
            assert_eq!(inode.is_regular_file(), *expected == FileType::RegularFile);
            assert_eq!(inode.is_symlink(), *expected == FileType::Symlink);
            assert_eq!(inode.is_chrdev(), *expected == FileType::ChrDev);
            assert_eq!(inode.is_blkdev(), *expected == FileType::BlkDev);
            assert_eq!(inode.is_fifo(), *expected == FileType::Fifo);
            assert_eq!(inode.is_sock(), *expected == FileType::Sock);
        }
    }

    #[test]
    fn rdev_decodes_legacy_sda2() {
        // Legacy 16-bit dev_t: rdev = (major << 8) | minor.
        // sda2 = (8, 2) -> 0x0802. Verify the new-encoding decoder
        // recovers the same pair (since major < 0x1000 && minor < 0x100,
        // the high bits in the new encoding are zero).
        let rdev: u32 = 0x0802;
        let buf = synth_compact(DataLayout::FlatPlain, S_IFBLK | 0o660, 0, rdev);
        let inode = Inode::parse(0, &buf).unwrap();
        assert_eq!(inode.rdev(), Some((8, 2)));
    }

    #[test]
    fn rdev_decodes_new_encoding_large_minor() {
        // 32-bit encoding: major = 0xABC, minor = 0x12345.
        // Encoded: ((minor & 0xFFF00) << 12) | ((major & 0xFFF) << 8) | (minor & 0xFF)
        //        = 0x12300000 | 0xABC00 | 0x45 = 0x123ABC45.
        let major = 0xABCu32;
        let minor = 0x12345u32;
        let rdev: u32 = ((minor & 0xFFF00) << 12) | ((major & 0xFFF) << 8) | (minor & 0xFF);
        assert_eq!(rdev, 0x123A_BC45);
        let buf = synth_compact(DataLayout::FlatPlain, S_IFCHR | 0o600, 0, rdev);
        let inode = Inode::parse(0, &buf).unwrap();
        assert_eq!(inode.rdev(), Some((major, minor)));
    }

    #[test]
    fn rdev_none_for_non_device() {
        for mode in [S_IFREG | 0o644, S_IFDIR | 0o755, S_IFLNK | 0o777] {
            let buf = synth_compact(DataLayout::FlatPlain, mode, 0, 0x0802);
            let inode = Inode::parse(0, &buf).unwrap();
            assert_eq!(inode.rdev(), None);
        }
    }

    #[test]
    fn iloc_math() {
        let sb_buf = crate::superblock::tests::synth_sb(12, 36, 4, 16);
        let sb = Superblock::parse(&sb_buf).unwrap();
        // meta_blkaddr=4, blocksize=4096 -> meta starts at 16384.
        // NID 36 -> +36*32 = 1152 -> 17536.
        assert_eq!(Inode::iloc(&sb, 36), 16384 + 1152);
    }
}
