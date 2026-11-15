//! A file's compressed data taken from one image into another as it is.
//!
//! An image built from the files of another -- the same tree with a few
//! files changed, say -- has no need to decompress a file it did not
//! change only to compress it again. [`Transplant::of`] describes the
//! physical clusters an open image stores a file in, and
//! [`crate::mkfs::Node::TransplantedFile`] writes them to the new image
//! under an index of its own. No byte of the file is decoded on the way.
//!
//! What can be taken is what the builder itself writes: LZ4 pclusters, in
//! logical clusters of one block, each a frame ending at the end of its
//! blocks (`EROFS_FEATURE_INCOMPAT_ZERO_PADDING`) or one block of the file
//! stored as it is. A file stored any other way is one [`Transplant::of`]
//! answers `None` for, and the caller reads it and writes it as any other
//! file.
//!
//! Spec: the compressed-data layout in the public EROFS on-disk-format
//! documentation
//! (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
//! Independent implementation.

use crate::decompress::Algorithm;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::Inode;
use crate::layout::DataLayout;
use crate::mkfs::Content;
use crate::superblock::{is_valid_blkszbits, EROFS_FEATURE_INCOMPAT_ZERO_PADDING};
use crate::zmap::{ZMap, Z_EROFS_LCLUSTER_TYPE_HEAD1, Z_EROFS_LCLUSTER_TYPE_PLAIN};

/// The most blocks' worth of bytes one pcluster holds: 1 MiB, the most
/// the kernel decompresses at once.
pub(crate) const MAX_PCLUSTER_BYTES: u64 = 1 << 20;

/// One physical cluster of a [`Transplant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pcluster {
    /// Where in the file the bytes it holds start. It holds the file up
    /// to where the next pcluster starts, or to the end of the file.
    pub offset: u64,
    /// The blocks it fills.
    pub blocks: u32,
    /// An LZ4 frame ending at the end of those blocks; or, when `false`,
    /// at most one block of the file as it is, from the start of the one
    /// block.
    pub compressed: bool,
}

/// A regular file as LZ4 pclusters that already exist: where in the file
/// each starts, and a reader over the blocks of them all, in file order.
#[derive(Debug)]
pub struct Transplant<'a> {
    pub(crate) blkszbits: u8,
    pub(crate) size: u64,
    pub(crate) pclusters: Vec<Pcluster>,
    pub(crate) blocks: Content<'a>,
}

impl<'a> Transplant<'a> {
    /// A file of `size` bytes held by `pclusters`, whose blocks, of
    /// `1 << blkszbits` bytes each, `blocks` reads one pcluster after
    /// another.
    ///
    /// Refused, with `BadInode`, are pclusters no file is made of: none at
    /// all, or not in file order from offset 0 to before `size`; two that
    /// start in one block of the file, which an index of one-block logical
    /// clusters cannot name; one of no blocks or of more than 1 MiB; a
    /// stored one that is not one block, or holds more than a block of
    /// the file; and blocks that are not as many as the pclusters fill.
    /// What the blocks decompress to is not checked: that is the file its
    /// source held.
    pub fn new(
        blkszbits: u8,
        size: u64,
        pclusters: Vec<Pcluster>,
        blocks: Content<'a>,
    ) -> Result<Self> {
        if !is_valid_blkszbits(blkszbits) {
            return Err(Error::BadSuperblock("blkszbits out of range"));
        }
        let bs = 1u64 << blkszbits;
        if pclusters.first().map(|p| p.offset) != Some(0) {
            return Err(Error::BadInode(
                "a transplanted file's pclusters do not start at offset 0",
            ));
        }
        let mut total = 0u64;
        for (at, p) in pclusters.iter().enumerate() {
            let end = pclusters.get(at + 1).map_or(size, |next| next.offset);
            if end <= p.offset || end > size {
                return Err(Error::BadInode(
                    "a transplanted file's pclusters are not in file order",
                ));
            }
            if pclusters
                .get(at + 1)
                .is_some_and(|next| next.offset / bs == p.offset / bs)
            {
                return Err(Error::BadInode("two pclusters start in one lcluster"));
            }
            let span = p.blocks as u64 * bs;
            if p.blocks == 0 || span > MAX_PCLUSTER_BYTES {
                return Err(Error::BadInode(
                    "a transplanted pcluster of no blocks, or of more than 1 MiB",
                ));
            }
            if !p.compressed && (p.blocks != 1 || end - p.offset > bs) {
                return Err(Error::BadInode(
                    "a stored pcluster that is not one block of the file",
                ));
            }
            total += p.blocks as u64;
        }
        if blocks.size() != total * bs {
            return Err(Error::BadInode(
                "transplanted blocks are not as many as their pclusters fill",
            ));
        }
        Ok(Self {
            blkszbits,
            size,
            pclusters,
            blocks,
        })
    }

    /// The pclusters `fs` stores `inode`'s data in; or `None` when it is
    /// not a file a transplant can describe, which has to be read and
    /// written as any other.
    ///
    /// One it can describe is a regular file that is not empty, compressed
    /// with LZ4 alone, in logical clusters of one block, in an image with
    /// `EROFS_FEATURE_INCOMPAT_ZERO_PADDING`; none of it inline after the
    /// inode (ztailpacking), in the packed inode (fragments), or the start
    /// of a pcluster another extent wrote (dedupe); its stored blocks not
    /// interlaced (`Z_EROFS_ADVISE_INTERLACED_PCLUSTER`), which lays one
    /// out from where in a block its extent starts and not from the start
    /// of the block, as the builder writes them; and every pcluster of it
    /// a size the kernel reads.
    ///
    /// The blocks are read from `fs` when the image taking them is built.
    pub fn of(fs: &'a Filesystem, inode: &Inode) -> Result<Option<Self>> {
        let sb = fs.superblock();
        let compressed = matches!(
            inode.format.layout,
            DataLayout::Compression | DataLayout::CompressionLegacy
        );
        if !inode.is_regular_file()
            || inode.size == 0
            || !compressed
            || sb.feature_incompat & EROFS_FEATURE_INCOMPAT_ZERO_PADDING == 0
        {
            return Ok(None);
        }
        let dev = fs.metadata_device();
        let zmap = ZMap::open(dev, sb, inode)?;
        // `mkfs.erofs` 1.9 marks a file interlaced only in an image with
        // fragments, where every compressed file carries a fragment too.
        if zmap.lclusterbits() != 0
            || zmap.has_inline_tail()
            || zmap.has_fragment()
            || zmap.has_interlaced_pcluster()
        {
            return Ok(None);
        }
        let bs = sb.block_size();

        let mut pclusters = Vec::new();
        let mut runs: Vec<Run> = Vec::new();
        let mut total = 0u64;
        let mut at = 0u64;
        while at < inode.size {
            let extent = zmap.pcluster_extent(dev, at)?;
            let compressed = match extent.cluster_type {
                Z_EROFS_LCLUSTER_TYPE_PLAIN => false,
                Z_EROFS_LCLUSTER_TYPE_HEAD1 => {
                    if zmap.header_algo(extent.cluster_type)? != Algorithm::Lz4 {
                        return Ok(None);
                    }
                    true
                }
                // A second algorithm's head.
                _ => return Ok(None),
            };
            let (end, blocks) = (extent.source_end_byte, extent.pcluster_block_count);
            if extent.source_start_byte != at
                || end <= at
                || extent.partial_ref
                || extent.device_id != 0
                || blocks == 0
                || blocks * bs > MAX_PCLUSTER_BYTES
                || extent.pcluster_blkaddr as u64 + blocks > sb.blocks as u64
                || (!compressed && (blocks != 1 || end - at > bs))
            {
                return Ok(None);
            }
            let blocks = blocks as u32;
            // Pclusters next to one another in the image are read at once.
            match runs.last_mut() {
                Some(run)
                    if run.blkaddr as u64 + run.blocks as u64 == extent.pcluster_blkaddr as u64
                        && (run.blocks as u64 + blocks as u64) * bs <= MAX_PCLUSTER_BYTES =>
                {
                    run.blocks += blocks;
                }
                _ => runs.push(Run {
                    blkaddr: extent.pcluster_blkaddr,
                    blocks,
                }),
            }
            pclusters.push(Pcluster {
                offset: at,
                blocks,
                compressed,
            });
            total += blocks as u64;
            at = end;
        }

        let reader = PclusterBlocks {
            fs,
            runs: runs.into_iter(),
            buf: Vec::new(),
            at: 0,
        };
        Self::new(
            sb.blkszbits,
            inode.size,
            pclusters,
            Content::new(total * bs, move || Ok(reader)),
        )
        .map(Some)
    }

    /// The block size of the pclusters, as its base-2 logarithm. Only an
    /// image built with the same block size can take them.
    pub fn blkszbits(&self) -> u8 {
        self.blkszbits
    }

    /// The file's length in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The pclusters, in file order.
    pub fn pclusters(&self) -> &[Pcluster] {
        &self.pclusters
    }
}

/// Blocks of an image that hold pclusters next to one another, read at
/// once.
struct Run {
    blkaddr: u32,
    blocks: u32,
}

/// The blocks of a file's pclusters, one after another.
struct PclusterBlocks<'a> {
    fs: &'a Filesystem,
    runs: std::vec::IntoIter<Run>,
    buf: Vec<u8>,
    at: usize,
}

impl std::io::Read for PclusterBlocks<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.buf.len() {
            let Some(run) = self.runs.next() else {
                return Ok(0);
            };
            let bs = self.fs.superblock().block_size();
            self.buf.resize((run.blocks as u64 * bs) as usize, 0);
            self.fs
                .read_data(0, run.blkaddr as u64 * bs, &mut self.buf)
                .map_err(std::io::Error::other)?;
            self.at = 0;
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}
