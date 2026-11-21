//! The three driver API layers of rust-fs-core (`fs_core::layers`) for
//! EROFS: [`ErofsVolume`] is the `VolumeAPI`, `PrimitiveAPI` and `FileAPI`,
//! and [`ErofsSalvage`] the salvage adapter.
//!
//! Everything here is built on the EROFS primitives in
//! [`crate::primitives`], which stay this driver's own: the generic layers
//! carry only what every format shares. EROFS is read-only by format, so
//! nothing here writes, and the format has no deleted state: the adapter
//! lists `DeletedCandidates` and `DeletedExtents` as missing.
//!
//! How EROFS maps onto the generic types (docs/three-layer-api.md):
//!
//! - A candidate is an inode slot that decodes (`primitives::scan_inodes`).
//!   Its location is the inode's 32 or 64 bytes, its detail the NID.
//! - A walk lists the source extents that hold the file's data, as stored:
//!   plain blocks, inline tails, and whole pclusters for compressed data,
//!   including the packed inode's pclusters for a fragment. `claimed` is
//!   the bytes the file stores on the source, not its size, because a
//!   compressed file stores fewer bytes than it holds and a hole stores
//!   none; materializing a walk gives the stored bytes, which are the
//!   file's bytes only when nothing is compressed.
//! - The tree comes from every directory candidate's entries.
//! - Damage is every finding of the primitives, from the superblock down
//!   through every directory, inode, data map, xattr and pcluster the root
//!   reaches.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;

use fs_core::layers::{
    check_bounds, ApiError, Candidate, CandidateKind, CandidateState, Capabilities, ClusterLayout,
    Damage, DamageKind, DamageReport, DirEntry, Encoding, Extent, ExtentWalk, FileAPI, Geometry,
    HealthState, Integrity, Metadata, NodeKind, OpenVolume, Parent, ParentClaim, PrimitiveAPI,
    Probe, Recognition, SalvageAdapter, SalvageError, ScanReport, Severity, Sink, Structure,
    StructureRole, SuperblockCopy, Timestamp, Tree, TreeNode, VolumeAPI, VolumeHealth,
    VolumeSupport, WalkEnd, WalkExtent,
};
use fs_core::BlockRead;

use crate::inode::{FileType, Inode};
use crate::primitives::{
    self, inspect_directory, inspect_inode, inspect_superblock, inspect_xattrs, read_extent,
    scan_inodes, walk_data, DataExtent, ExtentSource, Finding, SuperblockEvidence,
};
use crate::superblock::{
    Superblock, EROFS_FEATURE_INCOMPAT_SUPPORTED, EROFS_SUPER_BLOCK_SIZE, EROFS_SUPER_OFFSET,
};
use crate::Filesystem;

/// The format's name in the layers.
pub const FORMAT: &str = "erofs";

/// An error from the layers: a shared refusal, a device error, or this
/// driver's own.
#[derive(Debug)]
pub enum LayerError {
    /// A refusal every driver shares.
    Api(ApiError),
    /// The source could not be read.
    Device(fs_core::Error),
    /// The EROFS reader refused.
    Erofs(crate::Error),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(error) => error.fmt(f),
            Self::Device(error) => write!(f, "source read failed: {error}"),
            Self::Erofs(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LayerError {}

impl From<ApiError> for LayerError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<fs_core::Error> for LayerError {
    fn from(error: fs_core::Error) -> Self {
        Self::Device(error)
    }
}

impl From<crate::Error> for LayerError {
    fn from(error: crate::Error) -> Self {
        match error {
            crate::Error::Block(error) => Self::Device(error),
            other => Self::Erofs(other),
        }
    }
}

/// An EROFS volume through the three layers.
///
/// [`OpenVolume::open`] needs an image [`Filesystem::open`] accepts, and
/// offers all three layers. [`OpenVolume::open_for_salvage`] never fails:
/// it keeps whatever the superblock says, and offers `FileAPI` only when
/// the image also opens strictly.
pub struct ErofsVolume {
    source: Arc<dyn BlockRead>,
    evidence: SuperblockEvidence,
    geometry: Option<primitives::Geometry>,
    filesystem: Option<Filesystem>,
}

impl ErofsVolume {
    /// The superblock as found, with its findings.
    pub fn superblock_evidence(&self) -> &SuperblockEvidence {
        &self.evidence
    }

    /// Where the image's structures are, when the superblock states it.
    pub fn primitive_geometry(&self) -> Option<&primitives::Geometry> {
        self.geometry.as_ref()
    }

    /// The strict reader, when the image opened strictly.
    pub fn filesystem(&self) -> Option<&Filesystem> {
        self.filesystem.as_ref()
    }

    fn strict(&self) -> Result<&Filesystem, LayerError> {
        self.filesystem
            .as_ref()
            .ok_or(LayerError::Api(ApiError::Unsupported(
                "browsing a volume that did not open strictly",
            )))
    }

    fn build(source: Arc<dyn BlockRead>, filesystem: Option<Filesystem>) -> Self {
        let evidence = inspect_superblock(&*source);
        let geometry = evidence.geometry();
        Self {
            source,
            evidence,
            geometry,
            filesystem,
        }
    }
}

impl VolumeAPI for ErofsVolume {
    type Error = LayerError;
    type Superblock = Superblock;

    fn format(&self) -> &'static str {
        FORMAT
    }

    fn size_bytes(&self) -> u64 {
        self.source.size_bytes()
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<(), LayerError> {
        check_bounds(offset, output.len(), self.size_bytes())?;
        self.source.read_at(offset, output)?;
        Ok(())
    }

    fn geometry(&self) -> Result<Geometry, LayerError> {
        let source_len = self.size_bytes();
        let Some(geometry) = &self.geometry else {
            // No superblock to state one: the source in 512-byte units, and
            // no layout, so nothing is claimed about where blocks are.
            return Ok(Geometry {
                sector_size: 512,
                allocation_unit: 512,
                allocation_units: source_len / 512,
                declared_bytes: source_len,
                layout: None,
                label: None,
                serial: None,
            });
        };
        let sb = self
            .evidence
            .fields
            .as_ref()
            .expect("a geometry has fields");
        let blocks = u64::from(sb.blocks);
        let name = sb.volume_name_str();
        Ok(Geometry {
            sector_size: 512,
            allocation_unit: geometry.block_size,
            allocation_units: blocks,
            declared_bytes: blocks.saturating_mul(geometry.block_size),
            layout: Some(block_layout(geometry, Some(sb))),
            label: (!name.is_empty()).then(|| name.to_string()),
            serial: (sb.uuid != [0; 16])
                .then(|| sb.uuid.iter().map(|b| format!("{b:02x}")).collect()),
        })
    }

    fn superblock(&self) -> Result<Superblock, LayerError> {
        self.evidence
            .fields
            .clone()
            .ok_or(LayerError::Api(ApiError::NotFound(
                "the superblock: the source ends before it".into(),
            )))
    }

    fn superblocks(&self) -> Result<Vec<SuperblockCopy<Superblock>>, LayerError> {
        // EROFS keeps one superblock and no backup.
        Ok(vec![SuperblockCopy {
            copy: 0,
            location: Extent::new(EROFS_SUPER_OFFSET, EROFS_SUPER_BLOCK_SIZE as u64),
            superblock: self.evidence.fields.clone(),
            integrity: superblock_integrity(&self.evidence),
        }])
    }

    fn support(&self) -> Result<VolumeSupport, LayerError> {
        let unknown = self.evidence.fields.as_ref().map_or(0, |sb| {
            sb.feature_incompat & !EROFS_FEATURE_INCOMPAT_SUPPORTED
        });
        Ok(VolumeSupport {
            files: self.filesystem.is_some(),
            salvage: capabilities(),
            unsupported: (0..32)
                .filter(|bit| unknown & (1 << bit) != 0)
                .map(|bit| format!("feature_incompat bit {bit}"))
                .collect(),
        })
    }

    fn health(&self) -> Result<VolumeHealth, LayerError> {
        let mut findings: Vec<Damage> = self.evidence.findings.iter().map(damage).collect();
        if self.filesystem.is_none() && findings.is_empty() {
            findings.push(Damage {
                kind: DamageKind::Malformed,
                severity: Severity::Error,
                location: Extent::new(EROFS_SUPER_OFFSET, EROFS_SUPER_BLOCK_SIZE as u64),
                structure: "superblock".into(),
                detail: "the image does not open strictly".into(),
            });
        }
        let mut health = VolumeHealth::from_findings(findings);
        if self.geometry.is_none() && health.state == HealthState::Clean {
            health.state = HealthState::Unknown;
        }
        Ok(health)
    }
}

impl OpenVolume for ErofsVolume {
    type Source = Arc<dyn BlockRead>;

    fn probe(source: &dyn BlockRead) -> Result<Probe, fs_core::Error> {
        let evidence = inspect_superblock(source);
        let magic = evidence
            .fields
            .as_ref()
            .is_some_and(|sb| sb.magic == crate::superblock::EROFS_SUPER_MAGIC_V1);
        let recognition = if !magic {
            Recognition::No
        } else if evidence.findings.is_empty() {
            Recognition::Yes
        } else {
            Recognition::Damaged
        };
        Ok(Probe {
            format: FORMAT,
            recognition,
            declared_bytes: evidence.geometry().and_then(|g| {
                evidence
                    .fields
                    .as_ref()
                    .map(|sb| u64::from(sb.blocks) * g.block_size)
            }),
            detail: evidence
                .findings
                .iter()
                .map(|f| f.detail.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        })
    }

    fn open(source: Arc<dyn BlockRead>) -> Result<Self, LayerError> {
        let filesystem = Filesystem::open(source.clone())?;
        Ok(Self::build(source, Some(filesystem)))
    }

    fn open_for_salvage(source: Arc<dyn BlockRead>) -> Result<Self, LayerError> {
        let filesystem = Filesystem::open(source.clone()).ok();
        Ok(Self::build(source, filesystem))
    }
}

/// What a caller may suppose about an EROFS image whose superblock is gone:
/// block size, metadata start block and root NID.
pub type ErofsHypothesis = (u64, u32, u64);

impl PrimitiveAPI for ErofsVolume {
    type Adapter = ErofsSalvage;
    type Hypothesis = ErofsHypothesis;

    fn hypotheses(&self) -> Result<Vec<ErofsHypothesis>, SalvageError> {
        // What survives of the superblock's own fields, when anything does.
        Ok(inspect_superblock(&*self.source)
            .fields
            .filter(|sb| crate::superblock::is_valid_blkszbits(sb.blkszbits))
            .map(|sb| (sb.block_size(), sb.meta_blkaddr, u64::from(sb.root_nid)))
            .into_iter()
            .collect())
    }

    fn adapter_with(&self, h: &ErofsHypothesis) -> Result<ErofsSalvage, SalvageError> {
        let geometry = primitives::Geometry::hypothesis(h.0, h.1, h.2, self.source.size_bytes())
            .ok_or(SalvageError::NoGeometry)?;
        Ok(ErofsSalvage::from_hypothesis(geometry))
    }

    fn source(&self) -> &dyn BlockRead {
        &*self.source
    }

    fn adapter(&self) -> Result<ErofsSalvage, SalvageError> {
        ErofsSalvage::new(&*self.source)
    }

    fn structures(&self) -> Result<Vec<Structure>, SalvageError> {
        let mut out = vec![Structure {
            name: "superblock".into(),
            role: StructureRole::Superblock,
            location: Extent::new(EROFS_SUPER_OFFSET, EROFS_SUPER_BLOCK_SIZE as u64),
            copy: 0,
            integrity: superblock_integrity(&self.evidence),
        }];
        let Some(geometry) = &self.geometry else {
            return Ok(out);
        };
        let source_len = self.source.size_bytes();
        let in_source = |extent: Extent| extent.end().is_some_and(|end| end <= source_len);
        let root = inspect_inode(&*self.source, geometry, geometry.root_nid);
        let root_extent = Extent::new(root.offset, root.raw.len().max(32) as u64);
        if in_source(root_extent) {
            out.push(Structure {
                name: format!("root directory inode (nid {})", geometry.root_nid),
                role: StructureRole::Directory,
                location: root_extent,
                copy: 0,
                integrity: if root.findings.is_empty() && root.inode.is_some() {
                    Integrity::Unchecked
                } else {
                    Integrity::Failed
                },
            });
        }
        let meta = u64::from(geometry.meta_blkaddr) * geometry.block_size;
        if meta < source_len {
            out.push(Structure {
                name: "metadata area (inodes, inline data)".into(),
                role: StructureRole::RecordTable,
                location: Extent::new(meta, (source_len - meta).min(geometry.block_size)),
                copy: 0,
                integrity: Integrity::Unchecked,
            });
        }
        let xattrs = u64::from(geometry.xattr_blkaddr) * geometry.block_size;
        if geometry.xattr_blkaddr != 0 && xattrs < source_len {
            out.push(Structure {
                name: "shared xattr area".into(),
                role: StructureRole::Other,
                location: Extent::new(xattrs, (source_len - xattrs).min(geometry.block_size)),
                copy: 0,
                integrity: Integrity::Unchecked,
            });
        }
        Ok(out)
    }
}

impl FileAPI for ErofsVolume {
    type Node = u64;

    fn root(&self) -> Result<u64, LayerError> {
        Ok(self.strict()?.superblock().root_nid.into())
    }

    fn read_dir(&self, directory: &u64) -> Result<Vec<DirEntry<u64>>, LayerError> {
        let fs = self.strict()?;
        let inode = fs.read_inode(*directory)?;
        if !inode.is_dir() {
            return Err(ApiError::NotADirectory(format!("nid {directory}")).into());
        }
        let mut out = Vec::new();
        for entry in fs.read_dir(&inode)? {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let child = fs.read_inode(entry.nid)?;
            let name_bytes = entry.name.clone();
            let (name, lossy) = match String::from_utf8(entry.name) {
                Ok(name) => (name, false),
                Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
            };
            out.push(DirEntry {
                bytes: name_bytes,
                name,
                lossy,
                kind: node_kind(&child),
                size: child.size,
                node: entry.nid,
            });
        }
        Ok(out)
    }

    fn metadata(&self, node: &u64) -> Result<Metadata, LayerError> {
        let inode = self.strict()?.read_inode(*node)?;
        let mut metadata = Metadata::new(node_kind(&inode), inode.size);
        metadata.modified = Some(Timestamp {
            seconds: inode.mtime as i64,
            nanoseconds: inode.mtime_nsec,
        });
        metadata.mode = Some(u32::from(inode.mode));
        metadata.links = Some(u64::from(inode.nlink));
        Ok(metadata)
    }

    fn read_at(&self, file: &u64, offset: u64, output: &mut [u8]) -> Result<usize, LayerError> {
        let fs = self.strict()?;
        let inode = fs.read_inode(*file)?;
        if inode.is_dir() {
            return Err(ApiError::IsADirectory(format!("nid {file}")).into());
        }
        if offset >= inode.size {
            return Ok(0);
        }
        let count = (inode.size - offset).min(output.len() as u64) as usize;
        fs.read_file(&inode, offset, &mut output[..count])?;
        Ok(count)
    }

    fn read_link(&self, link: &u64) -> Result<Vec<u8>, LayerError> {
        let fs = self.strict()?;
        let inode = fs.read_inode(*link)?;
        if !inode.is_symlink() {
            return Err(ApiError::Unsupported("reading a link that is not a symlink").into());
        }
        Ok(fs.read_symlink_target(&inode)?)
    }
}

fn node_kind(inode: &Inode) -> NodeKind {
    match inode.file_type() {
        FileType::RegularFile => NodeKind::File,
        FileType::Dir => NodeKind::Directory,
        FileType::Symlink => NodeKind::Symlink,
        _ => NodeKind::Special,
    }
}

fn candidate_kind(inode: &Inode) -> CandidateKind {
    match node_kind(inode) {
        NodeKind::File => CandidateKind::File,
        NodeKind::Directory => CandidateKind::Directory,
        _ => CandidateKind::Other,
    }
}

fn capabilities() -> Capabilities {
    use fs_core::layers::Feature;
    Capabilities {
        missing: vec![Feature::DeletedCandidates, Feature::DeletedExtents],
    }
}

fn superblock_integrity(evidence: &SuperblockEvidence) -> Integrity {
    if !evidence.findings.is_empty() {
        Integrity::Failed
    } else if evidence.checksum_matches == Some(true) {
        Integrity::Verified
    } else {
        Integrity::Unchecked
    }
}

/// The generic damage a primitive's finding is.
pub fn damage(finding: &Finding) -> Damage {
    use primitives::Structure as S;
    let detail = finding.detail.as_str();
    let kind = if detail.starts_with("magic is") {
        DamageKind::Signature
    } else if detail.contains("checksum") {
        DamageKind::Checksum
    } else if detail.contains("cannot be read") {
        DamageKind::Unreadable
    } else if detail.contains("past the source's end")
        || detail.contains("the source ends")
        || detail.contains("ends before")
        || detail.contains("source holds only")
    {
        DamageKind::Missing
    } else if detail.contains("does not decompress") || detail.contains("does not decode") {
        DamageKind::Broken
    } else {
        DamageKind::Malformed
    };
    let structure = match finding.structure {
        S::Superblock => "superblock".to_string(),
        S::Inode { nid } => format!("inode {nid}"),
        S::DataMap { nid } => format!("data of inode {nid}"),
        S::Xattr { nid } => format!("xattrs of inode {nid}"),
        S::DirBlock { dir, index } => format!("block {index} of directory {dir}"),
        S::Source => "source".to_string(),
    };
    Damage {
        kind,
        severity: Severity::Error,
        location: Extent::new(finding.offset, finding.len.max(1)),
        structure,
        detail: finding.detail.clone(),
    }
}

fn block_layout(geometry: &primitives::Geometry, sb: Option<&Superblock>) -> ClusterLayout {
    let in_source = geometry.source_len / geometry.block_size;
    ClusterLayout {
        first: 0,
        count: sb.map_or(in_source, |sb| in_source.min(u64::from(sb.blocks))),
        size: geometry.block_size,
        offset: 0,
    }
}

/// The EROFS salvage adapter: the geometry and superblock fields of one
/// source, and no device.
#[derive(Clone, Debug)]
pub struct ErofsSalvage {
    geometry: primitives::Geometry,
    superblock: Option<Superblock>,
}

impl ErofsSalvage {
    /// The adapter for a source, from whatever geometry survives: the
    /// superblock's, or with its magic or checksum destroyed, the fields
    /// that still state a block size, as a hypothesis.
    /// `SalvageError::NoGeometry` when no block size survives; then
    /// [`ErofsSalvage::from_hypothesis`] takes the caller's.
    pub fn new(source: &dyn BlockRead) -> Result<Self, SalvageError> {
        let evidence = inspect_superblock(source);
        if let Some(geometry) = evidence.geometry() {
            return Ok(Self {
                geometry,
                superblock: evidence.fields,
            });
        }
        let sb = evidence.fields.ok_or(SalvageError::NoGeometry)?;
        if !crate::superblock::is_valid_blkszbits(sb.blkszbits) {
            return Err(SalvageError::NoGeometry);
        }
        let mut geometry = primitives::Geometry::hypothesis(
            sb.block_size(),
            sb.meta_blkaddr,
            u64::from(sb.root_nid),
            source.size_bytes(),
        )
        .ok_or(SalvageError::NoGeometry)?;
        geometry.xattr_blkaddr = sb.xattr_blkaddr;
        Ok(Self {
            geometry,
            superblock: Some(sb),
        })
    }

    /// The adapter for a source whose superblock is destroyed, from a
    /// geometry the caller supplies ([`primitives::Geometry::hypothesis`]).
    pub fn from_hypothesis(geometry: primitives::Geometry) -> Self {
        Self {
            geometry,
            superblock: None,
        }
    }

    fn geometry_for(&self, source: &dyn BlockRead) -> primitives::Geometry {
        // The adapter's geometry, bounded by the source it is given.
        primitives::Geometry {
            source_len: source.size_bytes(),
            ..self.geometry
        }
    }

    fn inode(&self, source: &dyn BlockRead, nid: u64) -> Option<Inode> {
        // Decoded even with findings: the walk reports where its map breaks.
        inspect_inode(source, &self.geometry_for(source), nid).inode
    }

    fn nid_at(&self, offset: u64) -> Option<u64> {
        let meta = u64::from(self.geometry.meta_blkaddr) * self.geometry.block_size;
        let rel = offset.checked_sub(meta)?;
        (rel % crate::inode::EROFS_INODE_SLOT_SIZE == 0)
            .then_some(rel / crate::inode::EROFS_INODE_SLOT_SIZE)
    }

    fn report_findings(
        findings: impl IntoIterator<Item = Finding>,
        sink: &mut dyn Sink<Damage>,
        report: &mut DamageReport,
    ) -> bool {
        for finding in findings {
            report.reported += 1;
            if sink.accept(damage(&finding)).is_break() {
                report.stopped = true;
                return false;
            }
        }
        true
    }
}

impl SalvageAdapter for ErofsSalvage {
    type Detail = u64;

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate<u64>>,
    ) -> Result<ScanReport, SalvageError> {
        let size = source.size_bytes();
        let end = range.end().ok_or(SalvageError::Bounds)?;
        if end > size {
            return Err(SalvageError::Bounds);
        }
        let geometry = self.geometry_for(source);
        let slot = crate::inode::EROFS_INODE_SLOT_SIZE;
        let meta = u64::from(geometry.meta_blkaddr) * geometry.block_size;
        let first = range.offset.saturating_sub(meta).div_ceil(slot);
        let last = end.saturating_sub(meta).div_ceil(slot);
        let mut report = ScanReport::default();
        let mut stopped = None;
        let scan = scan_inodes(source, &geometry, first..last, |evidence| {
            let inode = evidence.inode.as_ref().expect("candidates decode");
            let candidate = Candidate {
                location: Extent::new(evidence.offset, u64::from(inode.on_disk_size)),
                kind: candidate_kind(inode),
                state: CandidateState::Live,
                integrity: if evidence.findings.is_empty() {
                    Integrity::Unchecked
                } else {
                    Integrity::Failed
                },
                name: None,
                size: Some(inode.size),
                detail: evidence.nid,
            };
            let flow = sink.accept(candidate);
            if flow.is_break() {
                stopped = Some(evidence.nid);
            }
            flow
        });
        report.candidates = scan.candidates;
        report.unreadable = scan
            .findings
            .iter()
            .map(|f| Extent::new(f.offset, f.len))
            .collect();
        if stopped.is_some() {
            report.stopped_at = Some(geometry.inode_offset(scan.next));
        }
        Ok(report)
    }

    fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<u64>,
    ) -> Result<ExtentWalk, SalvageError> {
        if candidate.state == CandidateState::Deleted {
            return Err(SalvageError::Unsupported(
                fs_core::layers::Feature::DeletedExtents,
            ));
        }
        let geometry = self.geometry_for(source);
        let Some(inode) = self.inode(source, candidate.detail) else {
            return Ok(ExtentWalk {
                extents: Vec::new(),
                claimed: candidate.size.unwrap_or(0).max(1),
                end: WalkEnd::Broken,
            });
        };
        let walk = walk_data(source, &geometry, self.superblock.as_ref(), &inode);
        let mut runs: Vec<WalkExtent> = Vec::new();
        let mut end = WalkEnd::Complete;
        'extents: for extent in &walk.extents {
            let pieces: Vec<WalkExtent> = match &extent.source {
                ExtentSource::Fragment { packed_nid, offset } => {
                    // The tail lives in the packed inode's contents: each
                    // packed run it overlaps, placed at this file's offset.
                    let Some(packed) = self.inode(source, *packed_nid) else {
                        end = WalkEnd::Broken;
                        break;
                    };
                    let packed_walk =
                        walk_data(source, &geometry, self.superblock.as_ref(), &packed);
                    let (from, to) = (*offset, offset + extent.len);
                    let mut pieces = Vec::new();
                    for p in &packed_walk.extents {
                        let (a, b) = (from.max(p.logical), to.min(p.logical + p.len));
                        if a >= b {
                            continue;
                        }
                        match run(p, a, b, extent.logical + (a - from), geometry.source_len) {
                            Some(Some(piece)) => pieces.push(piece),
                            Some(None) => {}
                            None => {
                                end = WalkEnd::Broken;
                                break 'extents;
                            }
                        }
                    }
                    pieces
                }
                _ => match run(
                    extent,
                    extent.logical,
                    extent.logical + extent.len,
                    extent.logical,
                    geometry.source_len,
                ) {
                    Some(piece) => piece.into_iter().collect(),
                    None => {
                        end = WalkEnd::Broken;
                        break;
                    }
                },
            };
            runs.extend(pieces);
        }
        if !matches!(walk.end, primitives::WalkEnd::Complete) {
            end = WalkEnd::Broken;
        }
        Ok(ExtentWalk {
            extents: merge(runs),
            claimed: inode.size,
            end,
        })
    }

    fn read_encoded(
        &self,
        source: &dyn BlockRead,
        stored: Extent,
        encoding: &Encoding,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        if offset > encoding.length {
            return Err(SalvageError::Bounds);
        }
        let stored_end = stored.end().ok_or(SalvageError::Bounds)?;
        if stored_end > source.size_bytes() {
            return Err(SalvageError::Bounds);
        }
        let token = Pcluster::parse(&encoding.codec).ok_or(SalvageError::Unsupported(
            fs_core::layers::Feature::EncodedExtents,
        ))?;
        let geometry = self.geometry_for(source);
        let extent = DataExtent {
            logical: token.start + encoding.offset,
            len: encoding.length,
            source: ExtentSource::Pcluster {
                device: 0,
                offset: stored.offset,
                stored_len: token.stored_len,
                algorithm: token.algorithm,
                inline: token.inline,
                decoded_start: token.start,
                decoded_end: token.end,
                partial: token.partial,
                interlaced: token.interlaced,
            },
        };
        let bytes = read_extent(source, &geometry, self.superblock.as_ref(), 0, &extent)
            .map_err(|_| SalvageError::Unsupported(fs_core::layers::Feature::EncodedExtents))?;
        let count = (encoding.length - offset).min(output.len() as u64) as usize;
        output[..count].copy_from_slice(&bytes[offset as usize..offset as usize + count]);
        Ok(count)
    }

    fn rebuild_tree(
        &self,
        source: &dyn BlockRead,
        candidates: &[Candidate<u64>],
    ) -> Result<Tree, SalvageError> {
        let geometry = self.geometry_for(source);
        let by_nid: BTreeMap<u64, u64> = candidates
            .iter()
            .map(|c| (c.detail, c.location.offset))
            .collect();
        let mut parents: BTreeMap<u64, Vec<ParentClaim>> = BTreeMap::new();
        let mut directories: BTreeSet<u64> = BTreeSet::new();
        for candidate in candidates {
            if candidate.kind == CandidateKind::Directory {
                directories.insert(candidate.detail);
            }
        }
        for dir in directories {
            let Some(inode) = self.inode(source, dir) else {
                continue;
            };
            let parent = Parent::Candidate(by_nid[&dir]);
            for block in inspect_directory(source, &geometry, &inode) {
                for entry in block.entries {
                    if entry.name == b"." || entry.name == b".." {
                        continue;
                    }
                    if by_nid.contains_key(&entry.nid) {
                        let claim = ParentClaim {
                            parent,
                            name: Some(entry.name.clone()),
                            state: CandidateState::Live,
                        };
                        let claims = parents.entry(entry.nid).or_default();
                        if !claims.contains(&claim) {
                            claims.push(claim);
                        }
                    }
                }
            }
        }
        let nodes = candidates
            .iter()
            .map(|c| {
                let mut claims: Vec<ParentClaim> = if c.detail == geometry.root_nid {
                    vec![ParentClaim::live(Parent::Root)]
                } else {
                    Vec::new()
                };
                claims.extend(parents.get(&c.detail).into_iter().flatten().cloned());
                TreeNode {
                    candidate: c.location.offset,
                    parents: claims,
                }
            })
            .collect();
        Ok(Tree { nodes })
    }

    fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<Damage>,
    ) -> Result<DamageReport, SalvageError> {
        let geometry = self.geometry_for(source);
        let mut report = DamageReport::default();
        let evidence = inspect_superblock(source);
        if !Self::report_findings(evidence.findings, sink, &mut report) {
            return Ok(report);
        }
        // Everything the root reaches, each inode once.
        let mut queue = vec![geometry.root_nid];
        let mut visited = HashSet::new();
        while let Some(nid) = queue.pop() {
            if !visited.insert(nid) {
                continue;
            }
            let inode = inspect_inode(source, &geometry, nid);
            let Some(decoded) = inode.inode.clone() else {
                if !Self::report_findings(inode.findings, sink, &mut report) {
                    return Ok(report);
                }
                continue;
            };
            let mut findings = inode.findings;
            findings.extend(inspect_xattrs(source, &geometry, &decoded).findings);
            if decoded.is_dir() {
                for block in inspect_directory(source, &geometry, &decoded) {
                    findings.extend(block.findings);
                    for entry in block.entries {
                        if entry.name != b"." && entry.name != b".." {
                            queue.push(entry.nid);
                        }
                    }
                }
            } else {
                let walk = walk_data(source, &geometry, self.superblock.as_ref(), &decoded);
                for extent in &walk.extents {
                    if let Err(finding) =
                        read_extent(source, &geometry, self.superblock.as_ref(), nid, extent)
                    {
                        findings.push(finding);
                    }
                }
                if let primitives::WalkEnd::Broken(finding) = walk.end {
                    findings.push(finding);
                }
            }
            if !Self::report_findings(findings, sink, &mut report) {
                return Ok(report);
            }
        }
        Ok(report)
    }

    fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        let end = extent.end().ok_or(SalvageError::Bounds)?;
        if end > source.size_bytes() || offset > extent.length {
            return Err(SalvageError::Bounds);
        }
        let count = (extent.length - offset).min(output.len() as u64) as usize;
        source
            .read_at(extent.offset + offset, &mut output[..count])
            .map_err(SalvageError::Device)?;
        Ok(count)
    }

    fn cluster_layout(&self) -> Result<ClusterLayout, SalvageError> {
        Ok(block_layout(&self.geometry, self.superblock.as_ref()))
    }
}

/// Runs in file order, with each stored run that continues the one before it
/// in both the file and the source joined to it.
fn merge(runs: Vec<WalkExtent>) -> Vec<WalkExtent> {
    let mut out: Vec<WalkExtent> = Vec::new();
    for run in runs {
        if let Some(last) = out.last_mut() {
            if last.encoded.is_none()
                && run.encoded.is_none()
                && last.end() == Some(run.offset)
                && last.source.end() == Some(run.source.offset)
            {
                last.source.length += run.source.length;
                continue;
            }
        }
        out.push(run);
    }
    out
}

/// The file bytes `from..to` of a data run, in that run's own file
/// coordinates, placed at `file_at`: `Some(None)` for a hole, `None` when
/// the bytes are on another device.
fn run(
    p: &DataExtent,
    from: u64,
    to: u64,
    file_at: u64,
    source_len: u64,
) -> Option<Option<WalkExtent>> {
    let len = to - from;
    let within = from - p.logical;
    match &p.source {
        ExtentSource::Plain { device: 0, offset } | ExtentSource::Inline { offset } => Some(Some(
            WalkExtent::stored(file_at, Extent::new(offset + within, len)),
        )),
        ExtentSource::Plain { .. } | ExtentSource::Pcluster { device: 1.., .. } => None,
        ExtentSource::Hole => Some(None),
        ExtentSource::Fragment { .. } => None,
        ExtentSource::Pcluster {
            offset,
            stored_len,
            algorithm,
            inline,
            decoded_start,
            decoded_end,
            partial,
            interlaced,
            ..
        } => {
            let skip = from - decoded_start;
            if algorithm.is_none() && !interlaced {
                return Some(Some(WalkExtent::stored(
                    file_at,
                    Extent::new(offset + skip, len),
                )));
            }
            let span = crate::fs::pcluster_span(*decoded_start, *decoded_end).ok()? as u64;
            let token = Pcluster {
                algorithm: *algorithm,
                start: *decoded_start,
                end: *decoded_end,
                stored_len: *stored_len,
                partial: *partial,
                interlaced: *interlaced,
                inline: *inline,
            };
            let stored = (*stored_len).min(source_len.saturating_sub(*offset));
            Some(Some(WalkExtent {
                offset: file_at,
                source: Extent::new(*offset, stored),
                uninitialized: false,
                encoded: Some(Encoding {
                    codec: token.render(),
                    decoded: span,
                    offset: skip,
                    length: len,
                }),
            }))
        }
    }
}

/// What an EROFS pcluster codec token carries: the parameters
/// `primitives::read_extent` needs to decode the unit again.
struct Pcluster {
    algorithm: Option<crate::decompress::Algorithm>,
    start: u64,
    end: u64,
    stored_len: u64,
    partial: bool,
    interlaced: bool,
    inline: bool,
}

impl Pcluster {
    fn render(&self) -> String {
        let codec = match self.algorithm {
            Some(algorithm) => format!("erofs-{}", (algorithm as u8)),
            None => "erofs-interlaced".to_owned(),
        };
        format!(
            "{codec};start={};end={};stored={}{}{}{}",
            self.start,
            self.end,
            self.stored_len,
            if self.partial { ";partial" } else { "" },
            if self.interlaced { ";interlaced" } else { "" },
            if self.inline { ";inline" } else { "" },
        )
    }

    fn parse(token: &str) -> Option<Self> {
        let mut parts = token.split(';');
        let codec = parts.next()?;
        let algorithm = match codec.strip_prefix("erofs-")? {
            "interlaced" => None,
            id => Some(crate::decompress::Algorithm::from_id(id.parse().ok()?).ok()?),
        };
        let mut out = Pcluster {
            algorithm,
            start: 0,
            end: 0,
            stored_len: 0,
            partial: false,
            interlaced: false,
            inline: false,
        };
        for part in parts {
            match part.split_once('=') {
                Some(("start", v)) => out.start = v.parse().ok()?,
                Some(("end", v)) => out.end = v.parse().ok()?,
                Some(("stored", v)) => out.stored_len = v.parse().ok()?,
                None if part == "partial" => out.partial = true,
                None if part == "interlaced" => out.interlaced = true,
                None if part == "inline" => out.inline = true,
                _ => return None,
            }
        }
        Some(out)
    }
}

impl ErofsSalvage {
    /// The NID a candidate location names, for callers that hold only the
    /// location.
    pub fn nid_of(&self, location: Extent) -> Option<u64> {
        self.nid_at(location.offset)
    }
}
