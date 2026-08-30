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

/// The stride NIDs are counted in: an inode's byte offset is its NID
/// times this.
///
/// It shares its value with [`COMPACT_INODE_SIZE`] and is **not the
/// same quantity** — one is an addressing unit, the other a structure
/// size. They agree at 32 because a compact inode fills exactly one
/// slot; an extended inode occupies two. Naming both is the point;
/// collapsing them would lose the distinction that makes the extended
/// case legible.
pub const EROFS_INODE_SLOT_SIZE: u64 = 32;

/// Size of a compact (v1) on-disk inode.
pub const COMPACT_INODE_SIZE: u64 = 32;

/// Size of an extended (v2) on-disk inode — two slots.
pub const EXTENDED_INODE_SIZE: u64 = 64;

/// Bytes per superblock extension slot, counted by
/// `Superblock::sb_extslots`.
pub const SB_EXTSLOT_SIZE: u64 = 16;

/// Byte offsets within a compact (v1) on-disk inode, per
/// `struct erofs_inode_compact`.
///
/// Named for the reason given in [`crate::superblock::offsets`]: the
/// crate carried 117 unnamed inline hex ranges, and matching one
/// against the kernel header meant counting bytes.
///
/// The compact and extended layouts **diverge after offset 0x08**, and
/// they are separate tables here rather than one with exceptions —
/// `SIZE` is a `u32` at 0x08 in one and a `u64` at 0x08 in the other,
/// and `UID` is two bytes in one and four in the other. A single table
/// would have to encode that, and the encoding would be harder to
/// check against the header than two plain lists.
pub mod compact_offsets {
    use std::ops::Range;

    pub const FORMAT: Range<usize> = 0x00..0x02;
    pub const XATTR_ICOUNT: Range<usize> = 0x02..0x04;
    pub const MODE: Range<usize> = 0x04..0x06;
    pub const NLINK: Range<usize> = 0x06..0x08;
    pub const SIZE: Range<usize> = 0x08..0x0C;
    /// The format-dependent union: block address, raw device, or inline
    /// tail offset.
    pub const RAW_U: Range<usize> = 0x10..0x14;
    pub const INO: Range<usize> = 0x14..0x18;
    pub const UID: Range<usize> = 0x18..0x1A;
    pub const GID: Range<usize> = 0x1A..0x1C;
}

/// Byte offsets within an extended (v2) on-disk inode, per
/// `struct erofs_inode_extended`. See [`compact_offsets`] on why these
/// are two tables.
pub mod extended_offsets {
    use std::ops::Range;

    pub const FORMAT: Range<usize> = 0x00..0x02;
    pub const XATTR_ICOUNT: Range<usize> = 0x02..0x04;
    pub const MODE: Range<usize> = 0x04..0x06;
    pub const SIZE: Range<usize> = 0x08..0x10;
    /// The format-dependent union — same offset as the compact layout.
    pub const RAW_U: Range<usize> = 0x10..0x14;
    pub const INO: Range<usize> = 0x14..0x18;
    pub const UID: Range<usize> = 0x18..0x1C;
    pub const GID: Range<usize> = 0x1C..0x20;
    pub const MTIME: Range<usize> = 0x20..0x28;
    pub const MTIME_NSEC: Range<usize> = 0x28..0x2C;
    pub const NLINK: Range<usize> = 0x2C..0x30;
}

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
        if (bytes.len() as u64) < COMPACT_INODE_SIZE {
            return Err(Error::BadInode("buffer shorter than 32 bytes"));
        }
        let raw_format = u16::from_le_bytes(bytes[compact_offsets::FORMAT].try_into().unwrap());
        let format = InodeFormat::parse(raw_format)?;
        let xattr_icount =
            u16::from_le_bytes(bytes[compact_offsets::XATTR_ICOUNT].try_into().unwrap());
        let mode = u16::from_le_bytes(bytes[compact_offsets::MODE].try_into().unwrap());
        let raw_u = u32::from_le_bytes(bytes[compact_offsets::RAW_U].try_into().unwrap());

        match format.version {
            InodeVersion::Compact => {
                // size at 0x08 (u32), nlink at 0x06 (u16), uid at 0x18 (u16),
                // gid at 0x1A (u16), no mtime fields.
                let size =
                    u32::from_le_bytes(bytes[compact_offsets::SIZE].try_into().unwrap()) as u64;
                let nlink =
                    u16::from_le_bytes(bytes[compact_offsets::NLINK].try_into().unwrap()) as u32;
                let ino = u32::from_le_bytes(bytes[compact_offsets::INO].try_into().unwrap());
                let uid =
                    u16::from_le_bytes(bytes[compact_offsets::UID].try_into().unwrap()) as u32;
                let gid =
                    u16::from_le_bytes(bytes[compact_offsets::GID].try_into().unwrap()) as u32;
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
                    on_disk_size: COMPACT_INODE_SIZE as u8,
                })
            }
            InodeVersion::Extended => {
                if (bytes.len() as u64) < EXTENDED_INODE_SIZE {
                    return Err(Error::BadInode("extended inode buffer < 64 bytes"));
                }
                // size at 0x08 (u64), uid at 0x18 (u32), gid at 0x1C (u32),
                // mtime at 0x20 (u64), mtime_nsec 0x28 (u32), nlink 0x2C (u32).
                let size = u64::from_le_bytes(bytes[extended_offsets::SIZE].try_into().unwrap());
                let ino = u32::from_le_bytes(bytes[extended_offsets::INO].try_into().unwrap());
                let uid = u32::from_le_bytes(bytes[extended_offsets::UID].try_into().unwrap());
                let gid = u32::from_le_bytes(bytes[extended_offsets::GID].try_into().unwrap());
                let mtime = u64::from_le_bytes(bytes[extended_offsets::MTIME].try_into().unwrap());
                let mtime_nsec =
                    u32::from_le_bytes(bytes[extended_offsets::MTIME_NSEC].try_into().unwrap());
                let nlink = u32::from_le_bytes(bytes[extended_offsets::NLINK].try_into().unwrap());
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
                    on_disk_size: EXTENDED_INODE_SIZE as u8,
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
        // Read a full extended inode: the version is only knowable
        // after parsing, and a compact one simply ignores the tail.
        let mut buf = [0u8; EXTENDED_INODE_SIZE as usize];
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
    /// The inode offset tables, checked against the kernel header.
    ///
    /// A deliberate second copy, for the reason given in
    /// [`crate::superblock`]'s equivalent. The compact and extended
    /// layouts are asserted separately because they genuinely diverge
    /// after 0x08 — `SIZE` is four bytes in one and eight in the other,
    /// `UID` two and four.
    #[test]
    fn inode_offsets_match_the_kernel_header() {
        use super::{compact_offsets as ci, extended_offsets as xi};

        assert_eq!(ci::FORMAT, 0x00..0x02);
        assert_eq!(ci::XATTR_ICOUNT, 0x02..0x04);
        assert_eq!(ci::MODE, 0x04..0x06);
        assert_eq!(ci::NLINK, 0x06..0x08);
        assert_eq!(ci::SIZE, 0x08..0x0C);
        assert_eq!(ci::RAW_U, 0x10..0x14);
        assert_eq!(ci::INO, 0x14..0x18);
        assert_eq!(ci::UID, 0x18..0x1A);
        assert_eq!(ci::GID, 0x1A..0x1C);

        assert_eq!(xi::FORMAT, 0x00..0x02);
        assert_eq!(xi::XATTR_ICOUNT, 0x02..0x04);
        assert_eq!(xi::MODE, 0x04..0x06);
        assert_eq!(xi::SIZE, 0x08..0x10);
        assert_eq!(xi::RAW_U, 0x10..0x14);
        assert_eq!(xi::INO, 0x14..0x18);
        assert_eq!(xi::UID, 0x18..0x1C);
        assert_eq!(xi::GID, 0x1C..0x20);
        assert_eq!(xi::MTIME, 0x20..0x28);
        assert_eq!(xi::MTIME_NSEC, 0x28..0x2C);
        assert_eq!(xi::NLINK, 0x2C..0x30);
    }

    /// The fields the two layouts share sit at the same offsets.
    ///
    /// `format`, `xattr_icount`, `mode`, the union at 0x10 and `ino`
    /// are read *before* the version is known — the parser reads
    /// `format` to find out which layout it has. If those five ever
    /// diverged, the parser could not work at all, and this says so.
    #[test]
    fn the_two_inode_layouts_agree_on_the_fields_read_before_the_version() {
        use super::{compact_offsets as ci, extended_offsets as xi};
        assert_eq!(ci::FORMAT, xi::FORMAT);
        assert_eq!(ci::XATTR_ICOUNT, xi::XATTR_ICOUNT);
        assert_eq!(ci::MODE, xi::MODE);
        assert_eq!(ci::RAW_U, xi::RAW_U);
        assert_eq!(ci::INO, xi::INO);
    }

    /// Neither layout has a field running past the structure it lives
    /// in, and neither overlaps itself.
    #[test]
    fn no_inode_field_overlaps_its_neighbour() {
        use super::{compact_offsets as ci, extended_offsets as xi};

        let compact = [
            (ci::FORMAT.start, ci::FORMAT.len()),
            (ci::XATTR_ICOUNT.start, ci::XATTR_ICOUNT.len()),
            (ci::MODE.start, ci::MODE.len()),
            (ci::NLINK.start, ci::NLINK.len()),
            (ci::SIZE.start, ci::SIZE.len()),
            (ci::RAW_U.start, ci::RAW_U.len()),
            (ci::INO.start, ci::INO.len()),
            (ci::UID.start, ci::UID.len()),
            (ci::GID.start, ci::GID.len()),
        ];
        check_layout(&compact, COMPACT_INODE_SIZE as usize, "compact inode");

        let extended = [
            (xi::FORMAT.start, xi::FORMAT.len()),
            (xi::XATTR_ICOUNT.start, xi::XATTR_ICOUNT.len()),
            (xi::MODE.start, xi::MODE.len()),
            (xi::SIZE.start, xi::SIZE.len()),
            (xi::RAW_U.start, xi::RAW_U.len()),
            (xi::INO.start, xi::INO.len()),
            (xi::UID.start, xi::UID.len()),
            (xi::GID.start, xi::GID.len()),
            (xi::MTIME.start, xi::MTIME.len()),
            (xi::MTIME_NSEC.start, xi::MTIME_NSEC.len()),
            (xi::NLINK.start, xi::NLINK.len()),
        ];
        check_layout(&extended, EXTENDED_INODE_SIZE as usize, "extended inode");
    }

    fn check_layout(fields: &[(usize, usize)], size: usize, what: &str) {
        let mut reached = 0usize;
        for (start, width) in fields {
            assert!(
                *start >= reached,
                "{what}: field at {start:#x} overlaps the one ending at {reached:#x}"
            );
            reached = start + width;
            assert!(
                reached <= size,
                "{what}: field at {start:#x} runs past {size} bytes"
            );
        }
    }

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
