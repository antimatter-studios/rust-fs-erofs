//! PrimitiveAPI: the on-disk structures of an image, read without
//! trusting it.
//!
//! [`crate::Filesystem`] is the reader for a healthy image: it checks the
//! superblock, refuses what it cannot vouch for, and stops at the first
//! malformed structure. These functions are for an image that may be
//! damaged. Each one reads one structure and returns what it found there
//! -- the raw bytes, every field that decodes, and a [`Finding`] for each
//! thing that is wrong -- instead of an error. An error is kept for what
//! is not evidence about the image at all.
//!
//! Every function takes the source as [`BlockRead`], which cannot write:
//! reading a damaged image never changes it. The plan these belong to is
//! docs/three-layer-api.md.

use fs_core::BlockRead;

use crate::inode::{
    Inode, COMPACT_INODE_SIZE, EXTENDED_INODE_SIZE, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK,
    S_IFMT, S_IFREG, S_IFSOCK,
};
use crate::layout::DataLayout;
use crate::superblock::{
    is_valid_blkszbits, Superblock, EROFS_FEATURE_COMPAT_SB_CHKSUM,
    EROFS_FEATURE_INCOMPAT_SUPPORTED, EROFS_SUPER_BLOCK_SIZE, EROFS_SUPER_MAGIC_V1,
    EROFS_SUPER_OFFSET,
};

/// The structure a [`Finding`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Structure {
    /// The 128-byte superblock at byte 1024, and the checksum span after
    /// it.
    Superblock,
    /// The inode with this NID: its 32 or 64 bytes, inline xattrs and
    /// inline tail.
    Inode {
        /// The inode's NID.
        nid: u64,
    },
}

/// One thing found wrong with a structure, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The structure it is about.
    pub structure: Structure,
    /// First source byte the finding is about.
    pub offset: u64,
    /// Number of source bytes it is about.
    pub len: u64,
    /// What was found, in words.
    pub detail: String,
}

impl Finding {
    fn superblock(offset: u64, len: u64, detail: impl Into<String>) -> Self {
        Finding {
            structure: Structure::Superblock,
            offset,
            len,
            detail: detail.into(),
        }
    }
}

/// The superblock as found, whatever state it is in.
#[derive(Debug, Clone)]
pub struct SuperblockEvidence {
    /// Where it was read: always [`EROFS_SUPER_OFFSET`].
    pub offset: u64,
    /// The bytes read there: [`EROFS_SUPER_BLOCK_SIZE`] of them, or fewer
    /// when the source ends sooner or cannot be read.
    pub raw: Vec<u8>,
    /// Every field, as stored, when all 128 bytes were read -- whether or
    /// not they make sense. [`SuperblockEvidence::findings`] says which
    /// do not.
    pub fields: Option<Superblock>,
    /// Whether the superblock checksum matched: `None` when the image
    /// does not carry one, or the span it covers could not be read.
    pub checksum_matches: Option<bool>,
    /// Size of the source, in bytes.
    pub source_len: u64,
    /// Everything wrong with it. Empty for a superblock
    /// [`crate::Filesystem::open`] would accept, as far as the superblock
    /// alone can tell.
    pub findings: Vec<Finding>,
}

impl SuperblockEvidence {
    /// The geometry this superblock states, when its magic and block size
    /// are sound enough to state one.
    pub fn geometry(&self) -> Option<Geometry> {
        let sb = self.fields.as_ref()?;
        if sb.magic != EROFS_SUPER_MAGIC_V1 || !is_valid_blkszbits(sb.blkszbits) {
            return None;
        }
        Some(Geometry {
            block_size: sb.block_size(),
            meta_blkaddr: sb.meta_blkaddr,
            xattr_blkaddr: sb.xattr_blkaddr,
            root_nid: u64::from(sb.root_nid),
            source_len: self.source_len,
            from_superblock: true,
        })
    }
}

/// Where an image's structures are: what every other primitive needs to
/// find anything.
///
/// Normally the superblock's ([`SuperblockEvidence::geometry`]). EROFS
/// keeps no backup superblock, so when it is destroyed the geometry is a
/// hypothesis -- the block size above all -- and
/// [`Geometry::hypothesis`] records that it is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes per block.
    pub block_size: u64,
    /// Block where the metadata area (inodes, inline data) starts.
    pub meta_blkaddr: u32,
    /// Block where the shared xattr area starts.
    pub xattr_blkaddr: u32,
    /// NID of the root directory.
    pub root_nid: u64,
    /// Size of the source, in bytes.
    pub source_len: u64,
    /// True when the superblock stated it, false for a hypothesis.
    pub from_superblock: bool,
}

impl Geometry {
    /// A geometry the caller supplies when the superblock cannot.
    ///
    /// `None` for a block size EROFS cannot express (a power of two from
    /// 512 bytes to 64 KiB).
    pub fn hypothesis(
        block_size: u64,
        meta_blkaddr: u32,
        root_nid: u64,
        source_len: u64,
    ) -> Option<Geometry> {
        if !block_size.is_power_of_two() || !is_valid_blkszbits(block_size.trailing_zeros() as u8) {
            return None;
        }
        Some(Geometry {
            block_size,
            meta_blkaddr,
            xattr_blkaddr: 0,
            root_nid,
            source_len,
            from_superblock: false,
        })
    }

    /// Byte offset of the inode with this NID.
    pub fn inode_offset(&self, nid: u64) -> u64 {
        u64::from(self.meta_blkaddr)
            .saturating_mul(self.block_size)
            .saturating_add(nid.saturating_mul(crate::inode::EROFS_INODE_SLOT_SIZE))
    }
}

/// Read the superblock of a source that may be damaged.
///
/// Never fails: a source too short to hold a superblock, one that cannot
/// be read there, and every field that is out of range or disagrees with
/// the source are all [`Finding`]s. Checked, in the order reported:
///
/// - the 128 bytes can be read;
/// - the magic is EROFS's;
/// - the block size is one EROFS can express, and the directory block
///   size is the block size;
/// - the checksum matches, when the image carries one;
/// - every `feature_incompat` bit is one this crate reads;
/// - the image fits in the source, and the metadata area and root inode
///   lie inside both.
pub fn inspect_superblock<R: BlockRead + ?Sized>(source: &R) -> SuperblockEvidence {
    let source_len = source.size_bytes();
    let offset = EROFS_SUPER_OFFSET;
    let want = EROFS_SUPER_BLOCK_SIZE as u64;
    let mut evidence = SuperblockEvidence {
        offset,
        raw: Vec::new(),
        fields: None,
        checksum_matches: None,
        source_len,
        findings: Vec::new(),
    };

    let available = source_len.saturating_sub(offset).min(want);
    if available < want {
        evidence.findings.push(Finding::superblock(
            offset,
            want,
            format!("the source is {source_len} bytes and ends before the superblock does"),
        ));
    }
    if available == 0 {
        return evidence;
    }
    let mut raw = vec![0u8; available as usize];
    if let Err(error) = source.read_at(offset, &mut raw) {
        evidence.findings.push(Finding::superblock(
            offset,
            available,
            format!("the superblock cannot be read: {error}"),
        ));
        return evidence;
    }
    evidence.raw = raw;
    if available < want {
        return evidence;
    }

    let sb = Superblock::decode(&evidence.raw);
    let findings = &mut evidence.findings;
    let field = |range: std::ops::Range<usize>| (offset + range.start as u64, range.len() as u64);

    if sb.magic != EROFS_SUPER_MAGIC_V1 {
        let (at, len) = field(0x00..0x04);
        findings.push(Finding::superblock(
            at,
            len,
            format!(
                "magic is {:#010x}, not EROFS's {EROFS_SUPER_MAGIC_V1:#010x}",
                sb.magic
            ),
        ));
    }
    let block_size_sound = is_valid_blkszbits(sb.blkszbits);
    if !block_size_sound {
        findings.push(Finding::superblock(
            offset + 0x0C,
            1,
            format!(
                "blkszbits is {}; a block size must be 2^9 to 2^16 bytes",
                sb.blkszbits
            ),
        ));
    }
    if sb.dirblkbits != 0 {
        findings.push(Finding::superblock(
            offset + 0x5A,
            1,
            format!(
                "dirblkbits is {}; a directory block must be the filesystem block",
                sb.dirblkbits
            ),
        ));
    }

    if block_size_sound && sb.feature_compat & EROFS_FEATURE_COMPAT_SB_CHKSUM != 0 {
        let span = sb.checksum_span_len().max(EROFS_SUPER_BLOCK_SIZE);
        let mut bytes = vec![0u8; span];
        evidence.checksum_matches = match source.read_at(offset, &mut bytes) {
            Ok(()) => Some(sb.verify_checksum(&bytes)),
            Err(error) => {
                findings.push(Finding::superblock(
                    offset,
                    span as u64,
                    format!("the span the superblock checksum covers cannot be read: {error}"),
                ));
                None
            }
        };
        if evidence.checksum_matches == Some(false) {
            findings.push(Finding::superblock(
                offset,
                span as u64,
                format!(
                    "the checksum is {:#010x} and does not match the {span} bytes it covers",
                    sb.checksum
                ),
            ));
        }
    }

    let unknown = sb.feature_incompat & !EROFS_FEATURE_INCOMPAT_SUPPORTED;
    if unknown != 0 {
        findings.push(Finding::superblock(
            offset + 0x50,
            4,
            format!("feature_incompat bits {unknown:#x} are not ones this crate reads"),
        ));
    }

    if block_size_sound {
        let block_size = sb.block_size();
        let image_len = u64::from(sb.blocks).saturating_mul(block_size);
        if image_len > source_len {
            findings.push(Finding::superblock(
                offset + 0x24,
                4,
                format!(
                    "the image is {} blocks ({image_len} bytes) and the source holds only \
                     {source_len} bytes",
                    sb.blocks
                ),
            ));
        }
        let meta = u64::from(sb.meta_blkaddr).saturating_mul(block_size);
        if meta >= source_len {
            findings.push(Finding::superblock(
                offset + 0x28,
                4,
                format!(
                    "the metadata area starts at block {} (byte {meta}), past the source's end",
                    sb.meta_blkaddr
                ),
            ));
        } else {
            let root = meta.saturating_add(
                u64::from(sb.root_nid).saturating_mul(crate::inode::EROFS_INODE_SLOT_SIZE),
            );
            if root.saturating_add(crate::inode::COMPACT_INODE_SIZE) > source_len {
                findings.push(Finding::superblock(
                    offset + 0x0E,
                    2,
                    format!(
                        "the root inode (nid {}) is at byte {root}, past the source's end",
                        sb.root_nid
                    ),
                ));
            }
        }
    }

    evidence.fields = Some(sb);
    evidence
}

/// An inode as found at a NID, whatever state it is in.
#[derive(Debug, Clone)]
pub struct InodeEvidence {
    /// The NID asked for.
    pub nid: u64,
    /// Where it was read: [`Geometry::inode_offset`].
    pub offset: u64,
    /// The bytes read there: 64, or fewer when the source ends sooner.
    pub raw: Vec<u8>,
    /// The inode, decoded field by field, when its format word names a
    /// version and a data layout this crate knows -- whether or not the
    /// fields make sense. [`InodeEvidence::findings`] says which do not.
    /// A compact inode's `mtime` is left as stored, relative to the
    /// image's build time, which only the superblock holds.
    pub inode: Option<Inode>,
    /// Everything wrong with it.
    pub findings: Vec<Finding>,
}

/// Read the inode at `nid` of a source that may be damaged.
///
/// Never fails. Checked, in the order reported:
///
/// - the inode can be read, compact (32 bytes) or extended (64);
/// - its format word names a data layout;
/// - its mode names a file type;
/// - a directory is not empty, and a symlink's target is no longer than
///   a block;
/// - for `FLAT_INLINE`, its tail lies in one block, as the kernel
///   requires;
/// - for `FLAT_PLAIN` and `FLAT_INLINE`, the data blocks lie inside the
///   source.
pub fn inspect_inode<R: BlockRead + ?Sized>(
    source: &R,
    geometry: &Geometry,
    nid: u64,
) -> InodeEvidence {
    let offset = geometry.inode_offset(nid);
    let mut evidence = InodeEvidence {
        nid,
        offset,
        raw: Vec::new(),
        inode: None,
        findings: Vec::new(),
    };
    let finding = |at: u64, len: u64, detail: String| Finding {
        structure: Structure::Inode { nid },
        offset: at,
        len,
        detail,
    };

    let available = geometry
        .source_len
        .saturating_sub(offset)
        .min(EXTENDED_INODE_SIZE);
    if available < COMPACT_INODE_SIZE {
        evidence.findings.push(finding(
            offset,
            COMPACT_INODE_SIZE,
            format!(
                "the inode is at byte {offset}, and the source ends at {}",
                geometry.source_len
            ),
        ));
        return evidence;
    }
    let mut raw = vec![0u8; available as usize];
    if let Err(error) = source.read_at(offset, &mut raw) {
        evidence.findings.push(finding(
            offset,
            available,
            format!("the inode cannot be read: {error}"),
        ));
        return evidence;
    }
    evidence.raw = raw;

    let raw_format = u16::from_le_bytes([evidence.raw[0], evidence.raw[1]]);
    if raw_format & 1 == 1 && available < EXTENDED_INODE_SIZE {
        evidence.findings.push(finding(
            offset,
            EXTENDED_INODE_SIZE,
            format!("the inode is extended, and the source ends {available} bytes into it"),
        ));
        return evidence;
    }
    let inode = match Inode::parse(nid, &evidence.raw) {
        Ok(inode) => inode,
        Err(_) => {
            evidence.findings.push(finding(
                offset,
                2,
                format!(
                    "the format word {raw_format:#06x} names data layout {}, which EROFS does not have",
                    (raw_format >> 1) & 0b111
                ),
            ));
            return evidence;
        }
    };
    let findings = &mut evidence.findings;
    let body = u64::from(inode.on_disk_size);

    if !matches!(
        inode.mode & S_IFMT,
        S_IFREG | S_IFDIR | S_IFLNK | S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK
    ) {
        findings.push(finding(
            offset + 4,
            2,
            format!("mode {:#08o} names no file type", inode.mode),
        ));
    }
    if inode.is_dir() && inode.size == 0 {
        findings.push(finding(
            offset + 8,
            4,
            "a directory with no entries, not even . and ..".into(),
        ));
    }
    if inode.is_symlink() && inode.size > geometry.block_size {
        findings.push(finding(
            offset + 8,
            4,
            format!(
                "a symlink target of {} bytes, longer than a block",
                inode.size
            ),
        ));
    }

    let xattr_size = if inode.xattr_icount == 0 {
        0
    } else {
        12 + (u64::from(inode.xattr_icount) - 1) * 4
    };
    let body_end = offset + body + xattr_size;
    match inode.format.layout {
        DataLayout::FlatPlain | DataLayout::FlatInline => {
            let whole_blocks = if inode.format.layout == DataLayout::FlatInline {
                inode.size / geometry.block_size
            } else {
                inode.size.div_ceil(geometry.block_size)
            };
            let start = u64::from(inode.raw_u).saturating_mul(geometry.block_size);
            let end = start.saturating_add(whole_blocks.saturating_mul(geometry.block_size));
            if whole_blocks > 0 && end > geometry.source_len {
                findings.push(finding(
                    offset + 0x10,
                    4,
                    format!(
                        "data blocks {} to {} run past the source's end",
                        inode.raw_u,
                        u64::from(inode.raw_u) + whole_blocks - 1
                    ),
                ));
            }
            if inode.format.layout == DataLayout::FlatInline {
                // The kernel's rule (erofs_map_blocks_flatmode): the tail
                // starts where the inode and its xattrs end, and must lie
                // in one block -- that one, not the inode's, which an
                // extended inode at a block's end leaves behind.
                let tail = inode.size % geometry.block_size;
                if tail > 0 && body_end % geometry.block_size + tail > geometry.block_size {
                    findings.push(finding(
                        offset + 8,
                        4,
                        format!("an inline tail of {tail} bytes runs past the end of its block"),
                    ));
                }
            }
        }
        _ => {}
    }

    evidence.inode = Some(inode);
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_device::MemDev as SliceDevice;

    fn image() -> Vec<u8> {
        let root = crate::mkfs::Node::Dir {
            mode: crate::mkfs::DEFAULT_DIR_MODE,
            entries: Default::default(),
            meta: Default::default(),
            xattrs: Vec::new(),
        };
        crate::mkfs::build_image(root, 12).expect("build_image")
    }

    fn details(evidence: &SuperblockEvidence) -> Vec<&str> {
        evidence
            .findings
            .iter()
            .map(|f| f.detail.as_str())
            .collect()
    }

    #[test]
    fn a_healthy_superblock_has_no_findings_and_states_its_geometry() {
        let bytes = image();
        let evidence = inspect_superblock(&SliceDevice::new(bytes.clone()));
        assert_eq!(details(&evidence), Vec::<&str>::new());
        let geometry = evidence.geometry().expect("geometry");
        assert_eq!(geometry.block_size, 4096);
        assert!(geometry.from_superblock);
        assert_eq!(geometry.source_len, bytes.len() as u64);
        let sb = crate::superblock::Superblock::parse(&bytes[1024..1152]).unwrap();
        assert_eq!(geometry.root_nid, u64::from(sb.root_nid));
        assert_eq!(evidence.raw, bytes[1024..1152]);
    }

    #[test]
    fn a_destroyed_magic_is_reported_and_the_other_fields_are_kept() {
        let mut bytes = image();
        bytes[1024..1028].fill(0);
        let evidence = inspect_superblock(&SliceDevice::new(bytes));
        let found = details(&evidence);
        assert!(
            found.iter().any(|d| d.starts_with("magic is 0x00000000")),
            "{found:?}"
        );
        assert_eq!(evidence.findings[0].offset, 1024);
        assert_eq!(evidence.fields.as_ref().unwrap().blkszbits, 12);
        assert!(
            evidence.geometry().is_none(),
            "no geometry without the magic"
        );
    }

    #[test]
    fn a_checksum_that_does_not_match_is_reported() {
        let mut bytes = image();
        let sb = crate::superblock::Superblock::parse(&bytes[1024..1152]).unwrap();
        assert_ne!(
            sb.feature_compat & EROFS_FEATURE_COMPAT_SB_CHKSUM,
            0,
            "fixture: the builder writes a checksummed superblock"
        );
        // The volume name is inside the checksum span and read by nothing.
        bytes[1024 + 0x40] ^= 0xFF;
        let evidence = inspect_superblock(&SliceDevice::new(bytes));
        assert_eq!(evidence.checksum_matches, Some(false));
        assert!(details(&evidence)
            .iter()
            .any(|d| d.contains("does not match")));
        assert!(
            evidence.geometry().is_some(),
            "the geometry is still stated"
        );
    }

    #[test]
    fn a_truncated_source_is_reported_against_the_block_count() {
        let mut bytes = image();
        bytes.truncate(4096);
        let evidence = inspect_superblock(&SliceDevice::new(bytes));
        assert!(
            details(&evidence)
                .iter()
                .any(|d| d.contains("the source holds only 4096 bytes")),
            "{:?}",
            details(&evidence)
        );
    }

    #[test]
    fn a_source_too_short_for_a_superblock_is_a_finding_not_an_error() {
        let evidence = inspect_superblock(&SliceDevice::new(vec![0u8; 1100]));
        assert_eq!(evidence.raw.len(), 76);
        assert!(evidence.fields.is_none());
        assert!(details(&evidence)[0].contains("ends before the superblock does"));
    }

    #[test]
    fn an_impossible_block_size_and_unknown_feature_bits_are_each_reported() {
        let mut bytes = image();
        bytes[1024 + 0x0C] = 30;
        bytes[1024 + 0x50..1024 + 0x54].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        let evidence = inspect_superblock(&SliceDevice::new(bytes));
        let found = details(&evidence);
        assert!(
            found.iter().any(|d| d.starts_with("blkszbits is 30")),
            "{found:?}"
        );
        assert!(found.iter().any(|d| d.contains("0x80000000")), "{found:?}");
        assert!(evidence.geometry().is_none());
    }

    #[test]
    fn a_hypothesis_is_marked_as_one_and_refuses_impossible_block_sizes() {
        let g = Geometry::hypothesis(4096, 1, 36, 1 << 20).expect("4 KiB");
        assert!(!g.from_superblock);
        assert_eq!(g.inode_offset(36), 4096 + 36 * 32);
        assert!(Geometry::hypothesis(3000, 1, 0, 1 << 20).is_none());
        assert!(Geometry::hypothesis(256, 1, 0, 1 << 20).is_none());
    }

    fn tree_image() -> Vec<u8> {
        use crate::mkfs::{Node, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE};
        let file = |data: Vec<u8>| Node::File {
            mode: DEFAULT_FILE_MODE,
            data,
            meta: Default::default(),
            xattrs: Vec::new(),
        };
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("small.txt".to_string(), file(b"small\n".to_vec()));
        entries.insert("big.bin".to_string(), file(vec![7u8; 3 * 4096]));
        let root = Node::Dir {
            mode: DEFAULT_DIR_MODE,
            entries,
            meta: Default::default(),
            xattrs: Vec::new(),
        };
        crate::mkfs::build_image(root, 12).expect("build_image")
    }

    fn geometry_of(bytes: &[u8]) -> Geometry {
        inspect_superblock(&SliceDevice::new(bytes.to_vec()))
            .geometry()
            .expect("geometry")
    }

    fn nid_of(bytes: &[u8], path: &str) -> u64 {
        let fs = crate::Filesystem::open(std::sync::Arc::new(SliceDevice::new(bytes.to_vec())))
            .expect("open");
        fs.lookup_path(path).expect("lookup").nid
    }

    #[test]
    fn every_healthy_inode_decodes_as_the_reader_reads_it_with_no_findings() {
        let bytes = tree_image();
        let geometry = geometry_of(&bytes);
        let fs = crate::Filesystem::open(std::sync::Arc::new(SliceDevice::new(bytes.clone())))
            .expect("open");
        for path in ["/", "/small.txt", "/big.bin"] {
            let want = fs.lookup_path(path).expect("lookup");
            let evidence = inspect_inode(&SliceDevice::new(bytes.clone()), &geometry, want.nid);
            assert_eq!(evidence.findings, Vec::new(), "{path}");
            let got = evidence.inode.expect("decoded");
            assert_eq!(
                (got.mode, got.size, got.nlink, got.raw_u),
                (want.mode, want.size, want.nlink, want.raw_u),
                "{path}"
            );
            assert_eq!(evidence.offset, geometry.inode_offset(want.nid));
        }
    }

    #[test]
    fn an_unknown_data_layout_is_a_finding_and_keeps_the_raw_bytes() {
        let mut bytes = tree_image();
        let geometry = geometry_of(&bytes);
        let nid = nid_of(&bytes, "/small.txt");
        let at = geometry.inode_offset(nid) as usize;
        bytes[at] = (bytes[at] & 0b1) | (7 << 1);
        let evidence = inspect_inode(&SliceDevice::new(bytes), &geometry, nid);
        assert!(evidence.inode.is_none());
        assert_eq!(evidence.raw.len(), 64);
        assert_eq!(evidence.findings[0].offset, at as u64);
        assert!(evidence.findings[0].detail.contains("data layout 7"));
        assert_eq!(evidence.findings[0].structure, Structure::Inode { nid });
    }

    #[test]
    fn a_mode_without_a_file_type_is_a_finding_and_the_fields_are_kept() {
        let mut bytes = tree_image();
        let geometry = geometry_of(&bytes);
        let nid = nid_of(&bytes, "/small.txt");
        let at = geometry.inode_offset(nid) as usize;
        bytes[at + 4..at + 6].copy_from_slice(&0o644u16.to_le_bytes());
        let evidence = inspect_inode(&SliceDevice::new(bytes), &geometry, nid);
        assert_eq!(evidence.inode.as_ref().expect("decoded").size, 6);
        assert!(evidence.findings[0].detail.contains("names no file type"));
    }

    #[test]
    fn data_past_the_source_and_an_inline_tail_past_its_block_are_findings() {
        let bytes = tree_image();
        let geometry = geometry_of(&bytes);
        let big = nid_of(&bytes, "/big.bin");
        let mut moved = bytes.clone();
        let at = geometry.inode_offset(big) as usize;
        moved[at + 0x10..at + 0x14].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
        let evidence = inspect_inode(&SliceDevice::new(moved), &geometry, big);
        assert!(
            evidence
                .findings
                .iter()
                .any(|f| f.detail.contains("past the source's end")),
            "{:?}",
            evidence.findings
        );

        let small = nid_of(&bytes, "/small.txt");
        let mut long = bytes.clone();
        let at = geometry.inode_offset(small) as usize;
        let inline = crate::inode::Inode::parse(small, &long[at..at + 64]).unwrap();
        assert_eq!(inline.format.layout, DataLayout::FlatInline, "fixture");
        long[at + 8..at + 12].copy_from_slice(&4095u32.to_le_bytes());
        let evidence = inspect_inode(&SliceDevice::new(long), &geometry, small);
        assert!(
            evidence
                .findings
                .iter()
                .any(|f| f.detail.contains("inline tail")),
            "{:?}",
            evidence.findings
        );
    }

    #[test]
    fn an_inode_past_the_source_is_a_finding_not_an_error() {
        let bytes = tree_image();
        let geometry = geometry_of(&bytes);
        let evidence = inspect_inode(&SliceDevice::new(bytes), &geometry, 1 << 40);
        assert!(evidence.inode.is_none());
        assert!(evidence.findings[0].detail.contains("the source ends"));
    }
}
