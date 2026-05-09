//! Top-level read-only EROFS filesystem handle.
//!
//! Phase 0: FLAT_PLAIN + FLAT_INLINE only. Compressed and chunked
//! inodes return `Error::UnsupportedLayout`.

use crate::chunked::{self, EROFS_NULL_ADDR};
use crate::decompress::{self};
use crate::dir::{iter_block, DirEntry};
use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::layout::DataLayout;
use crate::superblock::{self, ComprCfgs, Superblock};
use crate::xattr::{self, XattrLongPrefix};
use crate::zmap::{self, Z_EROFS_LCLUSTER_TYPE_PLAIN};
use fs_core::BlockRead;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock};

/// Default capacity (in pcluster entries) for the decompression cache.
/// At a typical pcluster size of ≤ 256 KiB this caps cache memory at
/// roughly 64 MiB — generous for sequential-read workloads but bounded
/// enough that memory-constrained callers can opt down (or out via
/// `set_pcluster_cache_capacity(0)`). Picked empirically: every test
/// image's compressed inode fits in well under this; real-world
/// images with hundreds of multi-pcluster files still benefit from
/// the cache without unbounded growth.
pub const DEFAULT_PCLUSTER_CACHE_CAPACITY: usize = 256;

/// Internal state of the decompressed-pcluster LRU cache. Keyed by
/// `(inode.nid, pcluster_blkaddr)` so the same compressed payload at
/// the same on-disk blkaddr referenced by two different inodes never
/// collides — an unlikely but legal arrangement under BIG_PCLUSTER
/// where multiple inodes can share a blkaddr range.
struct PclusterCache {
    /// `None` when capacity was set to 0 (caching disabled). We keep
    /// the Mutex around so the read path can observe the disabled
    /// state cheaply without an outer Option dance.
    // The (nid, pcluster_idx) -> decompressed-block mapping is local to
    // this struct; introducing a top-level type alias would just push the
    // complexity around without aiding readability.
    #[allow(clippy::type_complexity)]
    inner: Option<LruCache<(u64, u32), Arc<Vec<u8>>>>,
    /// Counts cache hits, for tests + diagnostics. Never wraps in
    /// realistic workloads (u64 is fine).
    hits: u64,
    /// Counts cache misses (i.e. real decompress invocations). Pairs
    /// with `hits` for hit-rate calculation in tests / perf demos.
    misses: u64,
}

impl PclusterCache {
    fn new(capacity: usize) -> Self {
        let inner = NonZeroUsize::new(capacity).map(LruCache::new);
        PclusterCache {
            inner,
            hits: 0,
            misses: 0,
        }
    }
}

pub struct Filesystem {
    /// Primary backing device. All metadata reads (superblock, COMPR_CFGS
    /// blob, inodes, xattrs, the device table itself) go through this
    /// device. Data reads with `device_id == 0` also go here.
    primary: Arc<dyn BlockRead>,
    /// Extra backing devices for multi-device images. Indexed by
    /// `(device_id - 1)`; a `device_id == k > 0` in a chunkmap or zmap
    /// entry routes to `extras[k - 1]`. Stays empty for
    /// single-device images opened via [`Filesystem::open`].
    extras: Vec<Arc<dyn BlockRead>>,
    sb: Superblock,
    /// Lazily-loaded "packed inode" used when files carry the
    /// `Z_EROFS_ADVISE_FRAGMENT_PCLUSTER` advise bit. The packed
    /// inode is itself a regular compressed inode (often ztailpacked
    /// or compacted); we read it through the standard `read_file`
    /// path. We cache it via `OnceLock` so that fragment-bearing
    /// files don't re-read the inode body on every byte access.
    packed_inode: OnceLock<Inode>,
    /// Lazily-loaded custom xattr prefix dictionary. Read on first call
    /// to [`Filesystem::xattrs`] (or any other xattr resolver) and
    /// cached for the lifetime of the filesystem since dictionary
    /// contents don't change mid-image.
    xattr_prefix_dict: OnceLock<Vec<XattrLongPrefix>>,
    /// Parsed COMPR_CFGS blob (per-codec configuration). `None` means
    /// the SB doesn't advertise `EROFS_FEATURE_INCOMPAT_COMPR_CFGS`
    /// or the blob hasn't been read yet; populated on
    /// [`Filesystem::open`] so the read path can pass codec configs
    /// in without an extra device round-trip per cluster.
    compr_cfgs: Option<ComprCfgs>,
    /// LRU of decompressed pcluster bytes, keyed by
    /// `(inode.nid, pcluster_blkaddr)`. Sequential reads of a multi-
    /// block compressed inode otherwise pay the LZ4/LZMA/DEFLATE cost
    /// once per block; with the cache they pay once per pcluster.
    /// PLAIN clusters bypass the cache (already a single direct read).
    pcluster_cache: Mutex<PclusterCache>,
}

impl Filesystem {
    /// Open an EROFS image. Reads + validates the superblock.
    ///
    /// We accept images with `EROFS_FEATURE_INCOMPAT_FRAGMENTS` set
    /// even when `packed_nid == 0`: modern mkfs.erofs sets the
    /// feature bit speculatively (e.g. when `-Eztailpacking` alone
    /// is requested) and only populates `packed_nid` when an actual
    /// fragment-bearing inode is emitted. The malformedness check is
    /// therefore deferred to fragment-redirect time -- if a file's
    /// zmap header claims a fragment but `packed_nid` is still zero
    /// we return `Error::BadInode` from [`Self::packed_inode`].
    pub fn open(dev: Arc<dyn BlockRead>) -> Result<Self> {
        Self::open_with_devices(dev, Vec::new())
    }

    /// Multi-device open. The caller supplies the primary device plus
    /// one [`BlockRead`] per extra device the SB advertises (in
    /// `device_id` order: `extras[0]` is `device_id == 1`,
    /// `extras[1]` is `device_id == 2`, …).
    ///
    /// Validates `extras.len() == sb.extra_devices`; mismatches return
    /// [`Error::BadSuperblock`] with the `"extra device count
    /// mismatch"` reason. Use [`Filesystem::read_device_table`] before
    /// this call if you need to inspect the SB's tag/blocks fields to
    /// pick the right backings.
    pub fn open_with_devices(
        primary: Arc<dyn BlockRead>,
        extras: Vec<Arc<dyn BlockRead>>,
    ) -> Result<Self> {
        let sb = superblock::read(&*primary)?;
        if extras.len() != sb.extra_devices as usize {
            return Err(Error::BadSuperblock("extra device count mismatch"));
        }
        // Parse the COMPR_CFGS blob eagerly when the SB advertises it.
        // The blob is at most a few hundred bytes, sits at a fixed
        // offset, and the parsed result lives for the FS handle's
        // lifetime -- amortising the cost over every compressed-cluster
        // read. Errors here propagate (a malformed blob is a hard image
        // error; we'd rather fail open than risk wrong codec params).
        let compr_cfgs = superblock::read_compr_cfgs(&*primary, &sb)?;
        Ok(Filesystem {
            primary,
            extras,
            sb,
            packed_inode: OnceLock::new(),
            xattr_prefix_dict: OnceLock::new(),
            compr_cfgs,
            pcluster_cache: Mutex::new(PclusterCache::new(DEFAULT_PCLUSTER_CACHE_CAPACITY)),
        })
    }

    /// Read the on-disk device table. Returns [`superblock::DeviceSlot`]
    /// for each `device_id >= 1` (`device_id == 0` is the primary
    /// device and has no slot of its own). Useful for callers that
    /// need to inspect tags / sizes BEFORE opening the actual
    /// extra-device backings to pass into [`Self::open_with_devices`].
    pub fn read_device_table(&self) -> Result<Vec<superblock::DeviceSlot>> {
        superblock::read_device_table(&*self.primary, &self.sb)
    }

    /// Resolve `device_id` to a backing handle. `0` is the primary
    /// device; `>= 1` indexes into the extras table. Out-of-range ids
    /// return [`Error::BadSuperblock`].
    fn device_for(&self, device_id: u16) -> Result<&dyn BlockRead> {
        if device_id == 0 {
            Ok(&*self.primary)
        } else {
            let idx = device_id as usize - 1;
            self.extras
                .get(idx)
                .map(|a| a.as_ref())
                .ok_or(Error::BadSuperblock("device_id out of range"))
        }
    }

    /// Multi-device dispatch helper. Reads `buf.len()` bytes at byte
    /// offset `offset` from the device identified by `device_id`.
    /// `device_id == 0` is the primary device; `>= 1` routes through
    /// the extras table. Used by chunked / zmap read paths so the
    /// dispatch shape stays uniform across single- and multi-device
    /// images.
    pub fn read_block(&self, device_id: u16, offset: u64, buf: &mut [u8]) -> Result<()> {
        let dev = self.device_for(device_id)?;
        dev.read_at(offset, buf)?;
        Ok(())
    }

    /// Builder-style override of the decompressed-pcluster cache
    /// capacity. `0` disables caching entirely (useful for memory-
    /// constrained configs and for the cache's own benchmark tests).
    /// Replaces the existing cache wholesale, dropping any prior
    /// entries. Default capacity is [`DEFAULT_PCLUSTER_CACHE_CAPACITY`].
    pub fn with_pcluster_cache_capacity(self, n: usize) -> Self {
        *self.pcluster_cache.lock().expect("cache lock") = PclusterCache::new(n);
        self
    }

    /// Resize the decompressed-pcluster cache at runtime. `0` disables
    /// caching; any other value resets the cache to a fresh LRU of
    /// that capacity (existing entries dropped — simpler and safer
    /// than trying to migrate them between LRU instances).
    pub fn set_pcluster_cache_capacity(&self, n: usize) {
        *self.pcluster_cache.lock().expect("cache lock") = PclusterCache::new(n);
    }

    /// Diagnostics + test hook: returns
    /// `(entries, capacity, hits, misses)` for the decompressed-
    /// pcluster cache. `capacity == 0` means the cache is disabled and
    /// `entries` is always 0 in that mode. Surfaced as a public method
    /// so unit tests can assert that hot-path reads actually populate
    /// (and re-hit) the cache rather than silently re-decompressing.
    pub fn pcluster_cache_stats(&self) -> (usize, usize, u64, u64) {
        let g = self.pcluster_cache.lock().expect("cache lock");
        let (entries, capacity) = match &g.inner {
            Some(lru) => (lru.len(), lru.cap().get()),
            None => (0, 0),
        };
        (entries, capacity, g.hits, g.misses)
    }

    /// Decoded COMPR_CFGS blob, if the SB advertises it. Returns
    /// `None` when the feature bit is clear (the common case for
    /// images built before the LZMA-via-cfgs convention took hold).
    pub fn compr_cfgs(&self) -> Option<&ComprCfgs> {
        self.compr_cfgs.as_ref()
    }

    /// Lazily-load + cache the custom xattr prefix dictionary. Returns a
    /// borrowed slice; an empty dictionary is the common case (no
    /// `--xattr-prefix=` flags at mkfs time) and incurs no I/O after the
    /// first call.
    pub fn xattr_prefix_dict(&self) -> Result<&[XattrLongPrefix]> {
        if let Some(cached) = self.xattr_prefix_dict.get() {
            return Ok(cached.as_slice());
        }
        let dict = xattr::read_xattr_prefix_dictionary(&*self.primary, &self.sb)?;
        let _ = self.xattr_prefix_dict.set(dict);
        Ok(self.xattr_prefix_dict.get().expect("just set").as_slice())
    }

    /// Read all xattrs of an inode and resolve each entry's full
    /// (namespace-prefixed) name. Returns `(full_name, value)` pairs in
    /// on-disk order: inline entries first, shared entries second.
    ///
    /// Pulls inline xattrs and any shared-area entries referenced by the
    /// inline shared-index suffix, then resolves each entry's
    /// `name_index + name` through the cached prefix dictionary so
    /// custom-prefix attributes (e.g. those produced by
    /// `mkfs.erofs --xattr-prefix=`) come back with their full
    /// human-readable names like `user.dataitem.thing`.
    pub fn xattrs(&self, inode: &Inode) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let entries = xattr::read_all_xattrs(&*self.primary, &self.sb, inode)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let dict = self.xattr_prefix_dict()?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let full = xattr::resolve_with_dict(e.name_index, &e.name, dict)?;
            out.push((full, e.value));
        }
        Ok(out)
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    pub fn root_inode(&self) -> Result<Inode> {
        Inode::read(&*self.primary, &self.sb, self.sb.root_nid as u64)
    }

    pub fn read_inode(&self, nid: u64) -> Result<Inode> {
        Inode::read(&*self.primary, &self.sb, nid)
    }

    /// Lazily-load + cache the superblock's "packed inode" (the
    /// shared cross-file fragment store). Returns a borrowed
    /// reference so the caller can pass it through `read_file` to
    /// fetch fragment bytes. Only meaningful when the SB advertises
    /// `EROFS_FEATURE_INCOMPAT_FRAGMENTS`; we don't gate on the bit
    /// here because some images set the advise bit per-inode without
    /// the SB-level bit (tolerated reads, ergonomic for one-off
    /// writers — kernel mkfs.erofs always sets the SB bit).
    fn packed_inode(&self) -> Result<&Inode> {
        if let Some(cached) = self.packed_inode.get() {
            return Ok(cached);
        }
        if self.sb.packed_nid == 0 {
            return Err(Error::BadInode(
                "packed_nid is zero; cannot load packed inode",
            ));
        }
        let inode = Inode::read(&*self.primary, &self.sb, self.sb.packed_nid)?;
        // Race-resistant cache fill: if another caller raced ahead,
        // discard the local copy and use theirs.
        let _ = self.packed_inode.set(inode);
        Ok(self.packed_inode.get().expect("just set"))
    }

    /// List all entries under a directory inode. The directory's data
    /// is laid out as N consecutive blocks of dirents starting at
    /// `inode.raw_u * blocksize` for FLAT_PLAIN, or with the last block
    /// inlined for FLAT_INLINE.
    pub fn read_dir(&self, inode: &Inode) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        let bs = self.sb.block_size();
        let mut out = Vec::new();
        let mut remaining = inode.size;
        let mut block_idx: u64 = 0;
        let total_blocks = inode.size.div_ceil(bs);

        while remaining > 0 {
            let this_block = remaining.min(bs);
            let mut buf = vec![0u8; this_block as usize];
            self.read_data_block(inode, block_idx, total_blocks, &mut buf)?;
            // Pad short last blocks back up to bs for iter_block's NUL
            // scan -- iter_block expects to find the trailing zeros.
            if buf.len() < bs as usize {
                buf.resize(bs as usize, 0);
            }
            let entries = iter_block(&buf)?;
            out.extend(entries);
            remaining -= this_block;
            block_idx += 1;
        }
        Ok(out)
    }

    /// Look up a single name in a directory. Linear scan; EROFS sorts
    /// dirents by name hash on disk for binary search but Phase 0 stays
    /// simple.
    pub fn lookup(&self, dir: &Inode, name: &[u8]) -> Result<Inode> {
        for entry in self.read_dir(dir)? {
            if entry.name == name {
                return self.read_inode(entry.nid);
            }
        }
        Err(Error::NotFound)
    }

    /// Resolve a `/`-separated path starting at the root. Symlinks are
    /// returned as-is (not followed); use [`Filesystem::resolve_path`]
    /// for following.
    pub fn lookup_path(&self, path: &str) -> Result<Inode> {
        let mut node = self.root_inode()?;
        for component in path.split('/').filter(|c| !c.is_empty()) {
            node = self.lookup(&node, component.as_bytes())?;
        }
        Ok(node)
    }

    /// Read a symlink's target. EROFS stores the target string as the
    /// symlink inode's data, with `i_size` = byte length of the target.
    /// Layout is FLAT_INLINE for short targets and FLAT_PLAIN for longer
    /// ones; both are handled by the regular [`Filesystem::read_file`]
    /// path.
    ///
    /// Returns the raw bytes of the target (typically UTF-8, but EROFS
    /// preserves whatever bytes were stored at mkfs time).
    pub fn read_symlink_target(&self, inode: &Inode) -> Result<Vec<u8>> {
        if !inode.is_symlink() {
            return Err(Error::BadInode("read_symlink_target on non-symlink"));
        }
        let mut buf = vec![0u8; inode.size as usize];
        if !buf.is_empty() {
            self.read_file(inode, 0, &mut buf)?;
        }
        Ok(buf)
    }

    /// Resolve a path with optional symlink following.
    ///
    /// When `follow_symlinks` is false this is identical to
    /// [`Filesystem::lookup_path`] -- a leaf symlink is returned as-is.
    ///
    /// When `follow_symlinks` is true, every symlink encountered (both
    /// mid-path components and the leaf) is expanded: absolute targets
    /// restart resolution from the root, relative targets resolve
    /// against the symlink's parent directory.
    ///
    /// Loop / depth protection caps total symlink expansions at 40,
    /// matching Linux's `MAXSYMLINKS`. On overflow, returns
    /// `Error::BadInode("symlink loop")`.
    pub fn resolve_path(&self, path: &str, follow_symlinks: bool) -> Result<Inode> {
        if !follow_symlinks {
            return self.lookup_path(path);
        }

        const MAXSYMLINKS: u32 = 40;
        let mut budget: u32 = MAXSYMLINKS;

        // `pending`: components yet to consume, leftmost first.
        let mut pending: std::collections::VecDeque<String> = path
            .split('/')
            .filter(|c| !c.is_empty())
            .map(|s| s.to_string())
            .collect();
        // Path of components walked through so far (the directories
        // *containing* `node`). Empty == `node` is the root. A relative
        // symlink target resolves against this prefix.
        let mut walked: Vec<String> = Vec::new();
        let mut node = self.root_inode()?;

        while let Some(component) = pending.pop_front() {
            node = self.lookup(&node, component.as_bytes())?;
            if node.is_symlink() {
                if budget == 0 {
                    return Err(Error::BadInode("symlink loop"));
                }
                budget -= 1;
                let target = self.read_symlink_target(&node)?;
                let target_str = std::str::from_utf8(&target)
                    .map_err(|_| Error::BadInode("symlink target not UTF-8"))?;

                let target_components: Vec<String> = target_str
                    .split('/')
                    .filter(|c| !c.is_empty())
                    .map(|s| s.to_string())
                    .collect();

                if target_str.starts_with('/') {
                    // Absolute: restart from root, dropping walked.
                    walked.clear();
                    node = self.root_inode()?;
                } else {
                    // Relative: resolve against the symlink's parent
                    // directory, which is `walked` (we haven't pushed
                    // the symlink onto `walked` yet, since it's not a
                    // real path component to keep).
                    node = self.root_inode()?;
                    for c in &walked {
                        node = self.lookup(&node, c.as_bytes())?;
                    }
                }

                // Splice target components in front of pending so they
                // get consumed next.
                for c in target_components.into_iter().rev() {
                    pending.push_front(c);
                }
            } else {
                walked.push(component);
            }
        }
        Ok(node)
    }

    /// Read `buf.len()` bytes from a regular file inode starting at
    /// `offset`. Phase 0: FLAT_PLAIN + FLAT_INLINE. Returns `OutOfRange`
    /// if the read would extend past `inode.size`.
    pub fn read_file(&self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset.saturating_add(buf.len() as u64) > inode.size {
            return Err(Error::OutOfRange);
        }
        let bs = self.sb.block_size();
        let total_blocks = inode.size.div_ceil(bs);

        let mut written = 0usize;
        let mut cursor = offset;
        while written < buf.len() {
            let block_idx = cursor / bs;
            let block_start = block_idx * bs;
            // FLAT_INLINE's tail block is packed tight (no zero-pad to
            // bs), so the buffer must be sized to actual content.
            // FLAT_PLAIN tolerates either, since on-disk blocks are padded.
            let valid_in_block = (inode.size - block_start).min(bs) as usize;
            let in_block_off = (cursor % bs) as usize;
            let take = (valid_in_block - in_block_off).min(buf.len() - written);

            let mut block = vec![0u8; valid_in_block];
            self.read_data_block(inode, block_idx, total_blocks, &mut block)?;
            buf[written..written + take].copy_from_slice(&block[in_block_off..in_block_off + take]);

            written += take;
            cursor += take as u64;
        }
        Ok(())
    }

    /// Read the `block_idx`-th data block of an inode into `out` (which
    /// must be `<= block_size` bytes). `total_blocks` is the inode's
    /// rounded-up block count, used to spot the tail-inline block for
    /// FLAT_INLINE.
    fn read_data_block(
        &self,
        inode: &Inode,
        block_idx: u64,
        total_blocks: u64,
        out: &mut [u8],
    ) -> Result<()> {
        let bs = self.sb.block_size();
        match inode.format.layout {
            DataLayout::FlatPlain => {
                let off = inode.raw_u as u64 * bs + block_idx * bs;
                // FLAT_PLAIN data lives on the primary device — there
                // is no per-inode device_id slot for non-chunked layouts.
                self.read_block(0, off, out)
            }
            DataLayout::FlatInline => {
                if total_blocks > 0 && block_idx == total_blocks - 1 {
                    // Tail block: inline, sits at body_end on the primary.
                    let off = inode.body_end(&self.sb);
                    self.read_block(0, off, out)
                } else {
                    let off = inode.raw_u as u64 * bs + block_idx * bs;
                    self.read_block(0, off, out)
                }
            }
            DataLayout::ChunkBased => {
                let info = chunked::chunk_info(&self.sb, inode)?;
                // chunk_size in blocks = 1 << chunk_bits.
                let chunk_idx = block_idx >> info.chunk_bits;
                let block_in_chunk = block_idx & ((1u64 << info.chunk_bits) - 1);
                // Indexed chunkmap entries carry a per-chunk device_id;
                // route the data read through the matching backing.
                // The chunkmap itself is metadata and lives on the
                // primary device (same as inodes / xattrs).
                let (blkaddr, device_id) =
                    chunked::lookup_chunk_blkaddr(&*self.primary, &self.sb, inode, chunk_idx)?;
                if blkaddr == EROFS_NULL_ADDR {
                    // Hole: zero-fill.
                    out.fill(0);
                    return Ok(());
                }
                let off = (blkaddr as u64 + block_in_chunk) * bs;
                self.read_block(device_id, off, out)
            }
            DataLayout::Compression | DataLayout::CompressionLegacy => {
                self.read_compressed_block(inode, block_idx, out)
            }
        }
    }

    /// Decompress one block of a compressed inode. Phase 2 v0.3 supports
    /// legacy + compacted-2B index formats, ztailpacking, AND multi-
    /// lcluster pclusters (mkfs.erofs default since 1.5). Resolves the
    /// FULL pcluster owning `block_idx`, decompresses its entire source
    /// span at once, and copies the requested block out -- no caching.
    /// FRAGMENTS / BIG_PCLUSTER / compacted-1B variants are still flagged
    /// via [`zmap::ZMap::open`].
    ///
    /// Why this is harder than "decompress one lcluster": mkfs.erofs
    /// collates contiguous lclusters into a single LZ4/LZMA frame
    /// whenever the compressed bytes fit within one block. The HEAD
    /// lcluster's blkaddr then points at a frame whose decompressed
    /// output spans MULTIPLE lclusters' worth of source. Decompressing
    /// only one lcluster's worth produces plausible-looking but wrong
    /// bytes (LZ4 decompress_into is happy to stop early). The fix is
    /// to walk forward through NONHEAD entries until the next HEAD,
    /// compute the exact source span, and decompress the whole pcluster.
    fn read_compressed_block(&self, inode: &Inode, block_idx: u64, out: &mut [u8]) -> Result<()> {
        let bs = self.sb.block_size();
        let zmap = zmap::ZMap::open(&*self.primary, &self.sb, inode)?;
        let block_start = block_idx * bs;
        // A block can straddle a pcluster boundary (different mkfs
        // policies may not align pclusters to block boundaries). Loop
        // until we've filled the caller's buffer, advancing pcluster-
        // by-pcluster through whatever this block covers.
        let mut written: usize = 0;
        while written < out.len() {
            let cursor = block_start + written as u64;
            self.fill_from_one_pcluster(inode.nid, &zmap, cursor, &mut out[written..])
                .map(|n| written += n)?;
            if written == 0 {
                // Defensive: nothing copied means the resolver advanced
                // past end-of-file. Zero-fill the rest and exit.
                out[written..].fill(0);
                break;
            }
        }
        Ok(())
    }

    /// Resolve the pcluster containing `file_offset` and copy as many of
    /// its remaining bytes into `out` as fit. Returns the number of
    /// bytes written. Caller loops to span pcluster boundaries.
    ///
    /// `inode_nid` is folded into the cache key alongside the resolved
    /// `pcluster_blkaddr` so two inodes that happen to point at the
    /// same blkaddr (a possibility under BIG_PCLUSTER's shared-extent
    /// patterns) never serve each other's bytes.
    fn fill_from_one_pcluster(
        &self,
        inode_nid: u64,
        zmap: &zmap::ZMap<'_>,
        file_offset: u64,
        out: &mut [u8],
    ) -> Result<usize> {
        let bs = self.sb.block_size();
        if file_offset >= zmap.inode_size() {
            // Past EOF: zero-fill caller's remaining buffer (the read
            // loop already validated `offset + len <= inode.size`, but
            // a partial block beyond EOF is benign zero-padding).
            let n = out.len();
            out.fill(0);
            return Ok(n);
        }

        // FRAGMENT_PCLUSTER redirect: if the requested file_offset
        // falls inside this inode's fragment range, the bytes
        // logically live in the superblock's "packed inode" at
        // `fragmentoff + (file_offset - fragment_source_start)`.
        // This takes precedence over ztailpacking when both bits
        // are set on the same map header (per `ZMap::has_fragment`'s
        // doc). The packed inode is itself a regular compressed
        // inode (often ztailpacked); we recurse into the standard
        // `read_file` path to fetch its bytes.
        if let Some((fragmentoff, frag_start, frag_end)) = zmap.fragment_range(&*self.primary)? {
            if file_offset >= frag_start && file_offset < frag_end {
                let in_frag = file_offset - frag_start;
                let frag_remaining = (frag_end - file_offset) as usize;
                let take = out.len().min(frag_remaining);
                let packed = self.packed_inode()?;
                let src_off = fragmentoff as u64 + in_frag;
                if src_off.saturating_add(take as u64) > packed.size {
                    return Err(Error::BadInode(
                        "fragment range extends past packed inode size",
                    ));
                }
                self.read_file(packed, src_off, &mut out[..take])?;
                return Ok(take);
            }
        }

        let extent = zmap.pcluster_extent(&*self.primary, file_offset)?;
        let inline_tail = zmap.tail_inline_offset_and_size();
        let is_inline_tail_pc = extent.is_last_pcluster && inline_tail.is_some();
        let off_in_pcluster = (file_offset - extent.source_start_byte) as usize;
        let pcluster_remaining = (extent.source_end_byte - file_offset) as usize;
        let take = out.len().min(pcluster_remaining);

        if extent.cluster_type == Z_EROFS_LCLUSTER_TYPE_PLAIN && !is_inline_tail_pc {
            if zmap.has_interlaced_pcluster() {
                // INTERLACED PLAIN: source bytes are rotated within the
                // on-disk block. `clusterofs` is the rotation amount.
                // Reader reconstructs `source = on_disk[clusterofs..] ++
                // on_disk[..clusterofs]`. We only need the slice of
                // bytes covered by `[file_offset, file_offset + take)`,
                // but the rotation is across the WHOLE pcluster's
                // on-disk block range, so we materialise the rotated
                // source into a transient buffer and slice from it.
                //
                // Spec: `Z_EROFS_ADVISE_INTERLACED_PCLUSTER` semantics
                // described in the public EROFS on-disk-format
                // documentation
                // (<https://erofs.docs.kernel.org/en/latest/design.html>).
                let blocks = extent.pcluster_block_count;
                let on_disk_len = ((extent.source_end_byte - extent.source_start_byte) as usize)
                    .max(blocks as usize * bs as usize);
                // Source byte length matches the on-disk block range;
                // `pcluster_block_count` is the on-disk block count
                // and the rotated source occupies exactly that many
                // bytes. For non-BIG_PCLUSTER PLAIN the count is 1.
                let mut on_disk = vec![0u8; on_disk_len];
                let dev_off = extent.pcluster_blkaddr as u64 * bs;
                // Compressed pclusters route through the resolved
                // device_id (always 0 / primary under the public spec,
                // but plumbed for symmetry with chunked).
                self.read_block(extent.device_id, dev_off, &mut on_disk)?;
                let rot = extent.head_clusterofs as usize;
                if rot > on_disk_len {
                    return Err(Error::BadInode(
                        "INTERLACED PLAIN: clusterofs exceeds on-disk length",
                    ));
                }
                // Rotate-and-paste: source[i] = on_disk[(i + rot) % len].
                let mut source = vec![0u8; on_disk_len];
                source[..on_disk_len - rot].copy_from_slice(&on_disk[rot..]);
                source[on_disk_len - rot..].copy_from_slice(&on_disk[..rot]);
                out[..take].copy_from_slice(&source[off_in_pcluster..off_in_pcluster + take]);
                return Ok(take);
            }
            // PLAIN (non-interlaced): raw uncompressed bytes. The
            // pcluster is laid out contiguously starting at
            // `pcluster_blkaddr * bs` and covers
            // `[source_start_byte, source_end_byte)` of the file.
            let off = extent.pcluster_blkaddr as u64 * bs + off_in_pcluster as u64;
            self.read_block(extent.device_id, off, &mut out[..take])?;
            return Ok(take);
        }

        // Compressed: read the whole pcluster's compressed payload and
        // decompress it into a buffer sized to the full source span.
        // `h_algorithmtype` packs HEAD1 in the low nibble and HEAD2 in
        // the high nibble; `zmap.header_algo` does the per-cluster
        // dispatch so the read path doesn't have to re-derive nibbles.
        // Spec: public EROFS compression-format documentation
        // (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>).
        //
        // Cache fast path: a previously-decompressed pcluster is keyed
        // by `(inode_nid, pcluster_blkaddr)`. On a hit we bypass the
        // device read AND the codec invocation entirely, and just
        // memcpy the requested slice out of the cached `Arc<Vec<u8>>`.
        // This is the whole point of the cache — sequential block
        // reads of a multi-block pcluster otherwise re-decompress the
        // same payload on every call.
        let cache_key = (inode_nid, extent.pcluster_blkaddr);
        if let Some(cached) = self.cache_lookup(&cache_key) {
            // The cached buffer is sized to the pcluster's full source
            // span, so `off_in_pcluster + take <= cached.len()`.
            out[..take].copy_from_slice(&cached[off_in_pcluster..off_in_pcluster + take]);
            return Ok(take);
        }

        let algo = zmap.header_algo(extent.cluster_type)?;
        let (src_off, src_len) = if is_inline_tail_pc {
            let (off, sz) = inline_tail.expect("checked above");
            (off, sz as usize)
        } else {
            let blocks = extent.pcluster_block_count;
            // Cap reads at the end of the (routed) device so a generous
            // last-pcluster bound doesn't trip ShortRead.
            let dev = self.device_for(extent.device_id)?;
            let dev_size = dev.size_bytes();
            let off = extent.pcluster_blkaddr as u64 * bs;
            let want = blocks * bs;
            let capped = if off >= dev_size {
                0
            } else {
                want.min(dev_size - off)
            };
            (off, capped as usize)
        };
        let mut compressed = vec![0u8; src_len];
        if src_len > 0 {
            self.read_block(extent.device_id, src_off, &mut compressed)?;
        }

        let uncompressed_len = (extent.source_end_byte - extent.source_start_byte) as usize;
        let mut decompressed = vec![0u8; uncompressed_len];
        // For LZMA we plumb the COMPR_CFGS-derived dict_size / lc /
        // lp / pb through; for the other codecs the second arg is
        // ignored. `decompress_with_config` falls back to LZMA1
        // defaults when no config is present (ergonomic for older
        // images that strip the LZMA1 header but pre-date COMPR_CFGS).
        let lzma_cfg = self.compr_cfgs.as_ref().and_then(|c| c.lzma.as_ref());
        decompress::decompress_with_config(algo, lzma_cfg, &compressed, &mut decompressed)?;
        out[..take].copy_from_slice(&decompressed[off_in_pcluster..off_in_pcluster + take]);
        // Insert into cache AFTER the copy: the buffer is shared via
        // `Arc` so concurrent readers don't pay extra allocation, and
        // we don't hold the cache lock across the codec call.
        self.cache_insert(cache_key, Arc::new(decompressed));
        Ok(take)
    }

    /// Cache lookup that bumps the hit/miss counters and the LRU
    /// recency in one shot. Returns `None` when caching is disabled
    /// (capacity 0) or on a miss; the miss counter is only bumped
    /// when caching is actually live, so disabled-mode reads don't
    /// inflate the miss count and confuse the hit-rate stat.
    fn cache_lookup(&self, key: &(u64, u32)) -> Option<Arc<Vec<u8>>> {
        let mut g = self.pcluster_cache.lock().expect("cache lock");
        let lru = g.inner.as_mut()?;
        if let Some(buf) = lru.get(key) {
            let buf = Arc::clone(buf);
            g.hits += 1;
            Some(buf)
        } else {
            g.misses += 1;
            None
        }
    }

    /// Cache insert. No-op when caching is disabled (capacity 0).
    fn cache_insert(&self, key: (u64, u32), value: Arc<Vec<u8>>) {
        let mut g = self.pcluster_cache.lock().expect("cache lock");
        if let Some(lru) = g.inner.as_mut() {
            lru.put(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::{ftype, tests::synth_dir_block};
    use crate::inode::tests::synth_compact;
    use crate::layout::DataLayout;
    use crate::superblock::tests::synth_sb;
    use crate::superblock::EROFS_SUPER_OFFSET;
    use fs_core::{BlockRead, Result as BlockResult};
    use std::sync::Mutex;

    /// Tiny in-memory block device for tests. fs_core's SliceReader is a
    /// sub-range view, not an owner; FileDevice needs a tempfile. This is
    /// a pure-RAM owner backed by a Vec.
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

    /// Build a tiny image with:
    /// - blocksize 4 KiB
    /// - meta area at block 1
    /// - root dir inode at NID 0 with a single child file "hello.txt"
    /// - file at NID 1, FLAT_PLAIN, payload at block 2 = "hi\n" + zeros
    fn build_image() -> Vec<u8> {
        const BS: usize = 4096;
        // 4 blocks: 0 = SB area, 1 = meta (inodes + dir block), 2 = file data.
        // Put dir block in block 3 to keep things separated.
        let mut img = vec![0u8; BS * 4];

        // Superblock at byte 1024.
        let sb = synth_sb(12, 0, 1, 4); // root_nid=0, meta_blkaddr=1
        img[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        // Root dir inode at NID 0 in meta area = byte 4096.
        // FLAT_PLAIN, mode=dir, size = 4096 (one block of dirents),
        // raw_blkaddr = 3 (dir data block).
        let root = synth_compact(DataLayout::FlatPlain, 0x41ED, BS as u32, 3);
        img[BS..BS + 32].copy_from_slice(&root);

        // File inode at NID 1 = byte 4096 + 32 = 4128.
        // FLAT_PLAIN, mode=regular file, size=3, raw_blkaddr = 2.
        let file = synth_compact(DataLayout::FlatPlain, 0x81A4, 3, 2);
        img[BS + 32..BS + 64].copy_from_slice(&file);

        // File data at block 2 = byte 8192.
        img[2 * BS..2 * BS + 3].copy_from_slice(b"hi\n");

        // Dir block at block 3 = byte 12288, with one entry pointing at NID 1.
        let dir = synth_dir_block(&[(1, ftype::REG_FILE, b"hello.txt")], BS);
        img[3 * BS..4 * BS].copy_from_slice(&dir);

        img
    }

    #[test]
    fn open_and_read_root() {
        let img = build_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let root = fs.root_inode().unwrap();
        assert!(root.is_dir());
    }

    #[test]
    fn lookup_and_read_file() {
        let img = build_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.lookup_path("/hello.txt").unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size, 3);
        let mut buf = [0u8; 3];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"hi\n");
    }

    #[test]
    fn out_of_range_read_rejected() {
        let img = build_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.lookup_path("/hello.txt").unwrap();
        let mut buf = [0u8; 16];
        assert!(matches!(
            fs.read_file(&inode, 0, &mut buf),
            Err(Error::OutOfRange)
        ));
    }

    /// Build an image with a chunk-based file at NID 0 and verify the
    /// read path. Layout: blocksize 4 KiB, meta_blkaddr=1.
    ///
    /// File geometry: chunk_bits=0 (chunk == 1 block), 3 chunks total
    /// (size = 12 KiB). chunkmap is the 4-byte compact form, written
    /// inline immediately after the inode body. Three chunks:
    ///   chunk 0 -> data block 4 ("AAAA..."), chunk 1 -> hole
    ///   (NULL_ADDR), chunk 2 -> data block 5 ("CCCC...").
    fn build_chunked_image() -> Vec<u8> {
        const BS: usize = 4096;
        // 6 blocks: 0=SB area, 1=meta (inode + chunkmap), 2/3 unused,
        // 4=chunk0 data, 5=chunk2 data.
        let mut img = vec![0u8; BS * 6];

        let sb = synth_sb(12, 0, 1, 6);
        img[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        // Compact chunked inode at NID 0 with chunk_bits=0, INDEXES clear.
        // synth_compact gives raw_format = (ChunkBased<<1) = 0x08; we then
        // overwrite to set flags=0 explicitly (it already is 0).
        let mut inode_buf = synth_compact(DataLayout::ChunkBased, 0x81A4, (BS * 3) as u32, 0);
        // i_format already correct for flags=0; double-check by re-packing.
        let raw_format: u16 = (DataLayout::ChunkBased as u16) << 1;
        inode_buf[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        img[BS..BS + 32].copy_from_slice(&inode_buf);

        // Compact chunkmap (3 x __le32) immediately after the 32-byte inode.
        let map_off = BS + 32;
        img[map_off..map_off + 4].copy_from_slice(&4u32.to_le_bytes());
        img[map_off + 4..map_off + 8].copy_from_slice(&EROFS_NULL_ADDR.to_le_bytes());
        img[map_off + 8..map_off + 12].copy_from_slice(&5u32.to_le_bytes());

        // chunk 0 data at block 4: filled with 'A'.
        for b in &mut img[4 * BS..5 * BS] {
            *b = b'A';
        }
        // chunk 2 data at block 5: filled with 'C'.
        for b in &mut img[5 * BS..6 * BS] {
            *b = b'C';
        }

        img
    }

    #[test]
    fn chunked_read_present_chunks() {
        let img = build_chunked_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.read_inode(0).unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size, 4096 * 3);
        // First byte of chunk 0.
        let mut buf = [0u8; 4];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"AAAA");
        // First byte of chunk 2 (offset 8192).
        fs.read_file(&inode, 8192, &mut buf).unwrap();
        assert_eq!(&buf, b"CCCC");
    }

    #[test]
    fn chunked_read_hole_returns_zeros() {
        let img = build_chunked_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.read_inode(0).unwrap();
        // chunk 1 (offset 4096..8192) is a hole.
        let mut buf = [0xFFu8; 16];
        fs.read_file(&inode, 4096, &mut buf).unwrap();
        assert_eq!(&buf, &[0u8; 16]);
    }

    #[test]
    fn chunked_read_spanning_chunk_boundary() {
        let img = build_chunked_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.read_inode(0).unwrap();
        // Span the chunk0 -> chunk1 boundary: last 4 bytes of chunk 0
        // ("AAAA") + first 4 bytes of chunk 1 (hole, zeros).
        let mut buf = [0u8; 8];
        fs.read_file(&inode, 4096 - 4, &mut buf).unwrap();
        assert_eq!(&buf, b"AAAA\0\0\0\0");
        // Span chunk1 -> chunk2 boundary: last 4 of hole + first 4 of chunk 2.
        fs.read_file(&inode, 8192 - 4, &mut buf).unwrap();
        assert_eq!(&buf, b"\0\0\0\0CCCC");
    }

    /// Build a tiny image via [`crate::mkfs::build_image`] containing
    /// `/target.txt` with content "hello\n" and `/link` -> "target.txt".
    fn build_symlink_image() -> Vec<u8> {
        use crate::mkfs::{
            build_image, Node, NodeMeta, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE,
        };
        use std::collections::BTreeMap;
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        entries.insert(
            "target.txt".into(),
            Node::File {
                mode: DEFAULT_FILE_MODE,
                data: b"hello\n".to_vec(),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        );
        entries.insert(
            "link".into(),
            Node::Symlink {
                mode: DEFAULT_SYMLINK_MODE,
                target: "target.txt".into(),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        );
        build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap()
    }

    #[test]
    fn read_symlink_target_returns_target_string() {
        let img = build_symlink_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let link = fs.lookup_path("/link").unwrap();
        assert!(link.is_symlink());
        let target = fs.read_symlink_target(&link).unwrap();
        assert_eq!(target, b"target.txt");
    }

    #[test]
    fn read_symlink_target_rejects_non_symlink() {
        let img = build_symlink_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let regular = fs.lookup_path("/target.txt").unwrap();
        assert!(matches!(
            fs.read_symlink_target(&regular),
            Err(Error::BadInode(_))
        ));
    }

    #[test]
    fn resolve_path_no_follow_returns_symlink_inode() {
        let img = build_symlink_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.resolve_path("/link", false).unwrap();
        assert!(inode.is_symlink());
    }

    #[test]
    fn resolve_path_follow_resolves_to_target() {
        let img = build_symlink_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.resolve_path("/link", true).unwrap();
        assert!(inode.is_regular_file());
        assert_eq!(inode.size, 6);
        let mut buf = [0u8; 6];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello\n");
    }

    /// Build an image with `/a` -> "/a" -- an absolute self-loop.
    fn build_loop_image() -> Vec<u8> {
        use crate::mkfs::{build_image, Node, NodeMeta, DEFAULT_DIR_MODE, DEFAULT_SYMLINK_MODE};
        use std::collections::BTreeMap;
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        entries.insert(
            "a".into(),
            Node::Symlink {
                mode: DEFAULT_SYMLINK_MODE,
                target: "/a".into(),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        );
        build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap()
    }

    /// Synthetic image with an INTERLACED PLAIN pcluster covering one
    /// 4 KiB lcluster. Source bytes: 4096 bytes laid out as
    /// `0x00..0x10` for the first 16 bytes and `0xAA` for the rest.
    /// On-disk bytes: ROTATED such that bytes `[clusterofs..bs)` come
    /// first, then `[0..clusterofs)`. Reader's INTERLACED handling
    /// must rotate-and-paste back to the original.
    ///
    /// We choose `clusterofs = 100` arbitrarily; the on-disk block is
    /// `source[100..4096] ++ source[..100]`. The HEAD lcluster's
    /// `clusterofs` field carries 100, plumbed through
    /// `PclusterExtent::head_clusterofs`.
    #[test]
    fn interlaced_plain_cluster_round_trip() {
        use crate::zmap::{Z_EROFS_ADVISE_INTERLACED_PCLUSTER, Z_EROFS_LCLUSTER_TYPE_PLAIN};
        const BS: usize = 4096;
        // Build the source bytes (what the file should READ as).
        let mut source = vec![0xAAu8; BS];
        for (i, b) in source.iter_mut().enumerate().take(16) {
            *b = i as u8; // 0x00..0x0F at the start
        }
        // Last 16 bytes a unique marker so we can verify the wrap.
        for (i, b) in source.iter_mut().enumerate().skip(BS - 16) {
            *b = 0x80 | ((i - (BS - 16)) as u8); // 0x80..0x8F at the tail
        }
        let clusterofs: usize = 100;
        // On-disk = source rotated LEFT by clusterofs. I.e.,
        // on_disk[i] = source[(i + clusterofs) % BS], OR equivalently
        // on_disk = source[clusterofs..] ++ source[..clusterofs].
        // Wait — the spec says reader undoes via
        // `source = on_disk[clusterofs..] ++ on_disk[..clusterofs]`,
        // so on_disk = source[BS-clusterofs..] ++ source[..BS-clusterofs].
        // Let's verify: if on_disk = source rotated RIGHT by
        // `clusterofs` (i.e. on_disk[i] = source[(i - clusterofs + BS) % BS]),
        // then on_disk[clusterofs..] = source[0..BS-clusterofs] and
        // on_disk[..clusterofs] = source[BS-clusterofs..]. So the
        // reader's `on_disk[clusterofs..] ++ on_disk[..clusterofs]`
        // reconstructs source[0..BS-clusterofs] ++ source[BS-clusterofs..]
        // = source. Correct.
        let mut on_disk = vec![0u8; BS];
        on_disk[clusterofs..].copy_from_slice(&source[..BS - clusterofs]);
        on_disk[..clusterofs].copy_from_slice(&source[BS - clusterofs..]);

        // Layout: 4 blocks total.
        //   block 0: SB (offset 0; SB struct at 0x400).
        //   block 1: meta (root inode + file inode).
        //   block 2: data (the on-disk rotated bytes).
        //   block 3: dir block (entry "rotate.bin" -> NID 1).
        let mut img = vec![0u8; BS * 4];
        let sb = synth_sb(12, 0, 1, 4);
        img[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        // Root dir inode at NID 0 (byte BS): FlatPlain dir, size = BS,
        // raw_blkaddr = 3 (dir block).
        let root = synth_compact(DataLayout::FlatPlain, 0x41ED, BS as u32, 3);
        img[BS..BS + 32].copy_from_slice(&root);

        // File inode at NID 1 (byte BS+32): COMPRESSION (compact) layout
        // with size = BS. raw_u doesn't matter for compressed (the
        // pcluster blkaddr is in the lcluster index entry).
        // Compact zmap layout: header + a single compact-4B pack.
        let raw_format: u16 = (DataLayout::Compression as u16) << 1;
        let mut file = synth_compact(DataLayout::Compression, 0x81A4, BS as u32, 0);
        file[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        img[BS + 32..BS + 64].copy_from_slice(&file);

        // zmap header at body_end = BS+32+32 = BS+64. We only need
        // h_advise = INTERLACED bit set.
        let hdr_off = BS + 64;
        img[hdr_off + 4..hdr_off + 6]
            .copy_from_slice(&Z_EROFS_ADVISE_INTERLACED_PCLUSTER.to_le_bytes());
        // h_algorithmtype = 0; h_clusterbits = 0 (lclusterbits=0).

        // ebase = ALIGN(hdr_off, 8) + 8 = hdr_off + 8 (already aligned).
        // Compact pack: PLAIN with lo=clusterofs (100) at intra=0 only
        // (single lcluster). Need vcnt=2 in 4B pack; second slot can be
        // PLAIN/0 (unused since file has 1 lcluster).
        // Pack 0 = (PLAIN clusterofs=100, PLAIN 0), base = 1 (block 2 = blkaddr 2,
        // and HEAD/PLAIN lookup adds nblk=1: pcluster_blkaddr = base+nblk = 1+1 = 2).
        let pack_off = hdr_off + 8;
        let mut bs_buf = [0u8; 4];
        // write_packed_entry inlined: lobits=12, encodebits=16.
        // entry 0: type = PLAIN(0) << 12 | lo=100 = 0x064 = 100. Bits 0..16.
        // entry 1: type = PLAIN(0) << 12 | lo=0 = 0. Bits 16..32.
        let entry0: u32 = (Z_EROFS_LCLUSTER_TYPE_PLAIN as u32) << 12 | 100u32;
        let entry1: u32 = 0;
        // Write entry0 at bit 0 (4 bytes window).
        let combined: u32 = entry0 | (entry1 << 16);
        bs_buf.copy_from_slice(&combined.to_le_bytes());
        img[pack_off..pack_off + 4].copy_from_slice(&bs_buf);
        // Pack base = 1 (so PLAIN at intra=0 -> blkaddr = 1 + 1 = 2).
        img[pack_off + 4..pack_off + 8].copy_from_slice(&1u32.to_le_bytes());

        // Data block at block 2 = byte 2*BS.
        img[2 * BS..3 * BS].copy_from_slice(&on_disk);

        // Dir block at block 3.
        let dir = synth_dir_block(&[(1, ftype::REG_FILE, b"rotate.bin")], BS);
        img[3 * BS..4 * BS].copy_from_slice(&dir);

        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.lookup_path("/rotate.bin").unwrap();
        assert_eq!(inode.size as usize, BS);
        let mut buf = vec![0u8; BS];
        fs.read_file(&inode, 0, &mut buf).expect("interlaced read");
        assert_eq!(buf, source, "rotate-and-paste reconstruction");
        // Spot-check specific offsets that prove the rotation worked:
        // start of source (was at on_disk[clusterofs])
        assert_eq!(buf[0], 0x00);
        assert_eq!(buf[1], 0x01);
        // tail of source (was at on_disk[..clusterofs])
        assert_eq!(buf[BS - 16], 0x80);
        assert_eq!(buf[BS - 1], 0x8F);
    }

    #[test]
    fn resolve_path_symlink_loop_caps_at_40() {
        let img = build_loop_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let err = fs.resolve_path("/a", true).unwrap_err();
        match err {
            Error::BadInode(msg) => assert_eq!(msg, "symlink loop"),
            other => panic!("expected BadInode(\"symlink loop\"), got {other:?}"),
        }
    }

    // --- pcluster decompression cache tests ----------------------------
    //
    // These exercise the LRU that sits in front of the codec, verifying
    // that a compressed inode read twice in a row decompresses once,
    // that capacity 0 disables caching, that capacity 1 evicts as
    // expected, and that two different inodes pointing at the same
    // blkaddr (synthetic test) don't cross-contaminate via the cache.

    /// Build a multi-block compressed file via the in-tree mkfs. Returns
    /// the image bytes and the file's payload so a test can compare
    /// reads against ground truth. The payload is highly compressible
    /// so each lcluster's frame is well below an on-disk block,
    /// triggering the multi-lcluster pcluster path the cache benefits.
    fn build_compressed_image_for_cache(name: &str, payload: &[u8]) -> Vec<u8> {
        use crate::mkfs::{
            build_image, CompressedAlgo, CompressedFileSpec, Node, NodeMeta, DEFAULT_DIR_MODE,
            DEFAULT_FILE_MODE,
        };
        use std::collections::BTreeMap;
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        entries.insert(
            name.into(),
            Node::CompressedFile(CompressedFileSpec {
                mode: DEFAULT_FILE_MODE,
                data: payload.to_vec(),
                algo: CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        );
        build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap()
    }

    #[test]
    fn pcluster_cache_hit_serves_repeat_reads_without_redecompress() {
        // Multi-block compressible payload: 5 blocks of 'X'. With
        // lclusterbits=0 each block is one lcluster; the LZ4 frame
        // shrinks dramatically so multiple lclusters collate into a
        // single pcluster. Reading the file end-to-end thus invokes
        // the codec only once per pcluster -- the property the cache
        // is meant to preserve across REPEATED full reads.
        let bs = 4096usize;
        let payload = vec![b'X'; 5 * bs];
        let img = build_compressed_image_for_cache("c.bin", &payload);
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.lookup_path("/c.bin").unwrap();

        // First read: cache populates on each unique pcluster
        // (resulting in some misses) and intra-pcluster repeat block
        // accesses immediately hit the just-inserted entry. Both
        // counters can be non-zero on the first pass; the contract
        // we assert is "at least one miss happened" (the cache had
        // to get filled SOMEWHERE) and "at least one entry remains".
        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
        let (entries_after_first, capacity, hits_after_first, misses_after_first) =
            fs.pcluster_cache_stats();
        assert_eq!(capacity, DEFAULT_PCLUSTER_CACHE_CAPACITY);
        assert!(
            entries_after_first >= 1,
            "first read should populate at least one cache entry, got {entries_after_first}"
        );
        assert!(
            misses_after_first >= 1,
            "first read should record at least one miss (entries had to land somewhere)"
        );

        // Second read: every pcluster lookup must now be a hit; no
        // additional misses. Decompression effectively amortised away.
        let mut buf2 = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf2).unwrap();
        assert_eq!(buf2, payload);
        let (_, _, hits_after_second, misses_after_second) = fs.pcluster_cache_stats();
        assert!(
            hits_after_second > hits_after_first,
            "second read must register cache hits ({hits_after_first} -> {hits_after_second})"
        );
        assert_eq!(
            misses_after_second, misses_after_first,
            "second read must NOT decompress again ({misses_after_first} -> {misses_after_second})"
        );
    }

    #[test]
    fn pcluster_cache_evicts_at_capacity() {
        // Build two distinct compressed images; open one filesystem
        // per image (each image has its own NID space, but the cache
        // is per-FS so we use one FS per file by re-opening per
        // image). We squeeze the cache to capacity 1 and verify a
        // SECOND distinct pcluster read evicts the first entry,
        // observed via `pcluster_cache_stats` -- still 1 entry, but
        // a fresh re-read of the FIRST file's pcluster is a miss
        // (it would have been a hit at capacity >= 2).
        let bs = 4096usize;
        let payload_a = vec![b'A'; 5 * bs];

        // File A: read once with cap=1 to populate, then verify hit.
        let img_a = build_compressed_image_for_cache("a.bin", &payload_a);
        let dev_a: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img_a)));
        let fs_a = Filesystem::open(dev_a).unwrap();
        fs_a.set_pcluster_cache_capacity(1);
        let inode_a = fs_a.lookup_path("/a.bin").unwrap();

        // Populate cache from A.
        let mut buf = vec![0u8; payload_a.len()];
        fs_a.read_file(&inode_a, 0, &mut buf).unwrap();
        let (entries_a, cap_a, _, _) = fs_a.pcluster_cache_stats();
        assert_eq!(cap_a, 1);
        assert_eq!(
            entries_a, 1,
            "with capacity 1 the cache holds at most one entry"
        );

        // Now re-read A: should be a hit (still in the cache).
        let (_, _, hits_before, _) = fs_a.pcluster_cache_stats();
        fs_a.read_file(&inode_a, 0, &mut buf).unwrap();
        let (_, _, hits_after, _) = fs_a.pcluster_cache_stats();
        assert!(hits_after > hits_before, "A still cached -> hits must grow");

        // File B: separate FS, separate cache. We use the same
        // capacity-1 setting and read B, then read A AGAIN through B's
        // FS-handle-equivalent: actually we test eviction within ONE
        // FS by reading TWO different files. To do that, build a
        // single image containing both compressed files and read both
        // through one fs.
        let payload_a2 = vec![b'A'; 5 * bs];
        let payload_b2 = vec![b'B'; 5 * bs];
        // Build combined image with two compressed files.
        use crate::mkfs::{
            build_image, CompressedAlgo, CompressedFileSpec, Node, NodeMeta, DEFAULT_DIR_MODE,
            DEFAULT_FILE_MODE,
        };
        use std::collections::BTreeMap;
        let mk_node = |data: Vec<u8>| -> Node {
            Node::CompressedFile(CompressedFileSpec {
                mode: DEFAULT_FILE_MODE,
                data,
                algo: CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
            })
        };
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        entries.insert("a.bin".into(), mk_node(payload_a2.clone()));
        entries.insert("b.bin".into(), mk_node(payload_b2.clone()));
        let img_combined = build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap();
        let dev_c: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img_combined)));
        let fs = Filesystem::open(dev_c).unwrap();
        fs.set_pcluster_cache_capacity(1);
        let i_a = fs.lookup_path("/a.bin").unwrap();
        let i_b = fs.lookup_path("/b.bin").unwrap();

        let mut buf_a = vec![0u8; payload_a2.len()];
        fs.read_file(&i_a, 0, &mut buf_a).unwrap();
        assert_eq!(buf_a, payload_a2);

        // Now read B -- this should EVICT A (capacity = 1).
        let mut buf_b = vec![0u8; payload_b2.len()];
        fs.read_file(&i_b, 0, &mut buf_b).unwrap();
        assert_eq!(buf_b, payload_b2);

        let (_, _, _, misses_before) = fs.pcluster_cache_stats();
        // Re-read A: must be a MISS now (A was evicted by B).
        fs.read_file(&i_a, 0, &mut buf_a).unwrap();
        let (_, _, _, misses_after) = fs.pcluster_cache_stats();
        assert!(
            misses_after > misses_before,
            "evicted A should re-MISS the cache after B displaced it ({misses_before} -> {misses_after})"
        );
    }

    #[test]
    fn pcluster_cache_disabled_at_capacity_zero() {
        let bs = 4096usize;
        let payload = vec![b'Z'; 5 * bs];
        let img = build_compressed_image_for_cache("c.bin", &payload);
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        fs.set_pcluster_cache_capacity(0);
        let inode = fs.lookup_path("/c.bin").unwrap();

        let mut buf = vec![0u8; payload.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
        let (entries, capacity, hits, misses) = fs.pcluster_cache_stats();
        assert_eq!(entries, 0, "disabled cache must hold zero entries");
        assert_eq!(capacity, 0, "disabled cache reports zero capacity");
        assert_eq!(hits, 0, "disabled cache yields zero hits");
        assert_eq!(
            misses, 0,
            "disabled cache shouldn't bump miss counter either"
        );

        // Re-read still works (just slow): bytes must be correct.
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(buf, payload);
        let (_, _, hits2, _) = fs.pcluster_cache_stats();
        assert_eq!(hits2, 0, "still no hits after re-read with cache disabled");
    }

    #[test]
    fn pcluster_cache_independent_per_inode() {
        // Two compressed files in one image, each with its own NID.
        // We populate the cache from one and confirm the other still
        // misses on its first read -- proving the (nid, blkaddr)
        // composite key prevents cross-inode bleed even when blkaddrs
        // happen to coincide. (mkfs assigns distinct blkaddrs here, so
        // the test's stronger guarantee is that "different NIDs always
        // miss independently"; cf. the synthetic same-blkaddr test
        // below in the integration suite.)
        let bs = 4096usize;
        let payload_a = vec![b'A'; 3 * bs];
        let payload_b = vec![b'B'; 3 * bs];

        use crate::mkfs::{
            build_image, CompressedAlgo, CompressedFileSpec, Node, NodeMeta, DEFAULT_DIR_MODE,
            DEFAULT_FILE_MODE,
        };
        use std::collections::BTreeMap;
        let mk_node = |data: Vec<u8>| -> Node {
            Node::CompressedFile(CompressedFileSpec {
                mode: DEFAULT_FILE_MODE,
                data,
                algo: CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
            })
        };
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        entries.insert("a.bin".into(), mk_node(payload_a.clone()));
        entries.insert("b.bin".into(), mk_node(payload_b.clone()));
        let img = build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let i_a = fs.lookup_path("/a.bin").unwrap();
        let i_b = fs.lookup_path("/b.bin").unwrap();
        assert_ne!(i_a.nid, i_b.nid, "two files must have distinct NIDs");

        // Read A first -- populates cache for NID(a).
        let mut buf_a = vec![0u8; payload_a.len()];
        fs.read_file(&i_a, 0, &mut buf_a).unwrap();
        let (_, _, _, misses_after_a) = fs.pcluster_cache_stats();

        // Read B -- must MISS (different NID, distinct cache key).
        let mut buf_b = vec![0u8; payload_b.len()];
        fs.read_file(&i_b, 0, &mut buf_b).unwrap();
        assert_eq!(buf_b, payload_b);
        let (_, _, _, misses_after_b) = fs.pcluster_cache_stats();
        assert!(
            misses_after_b > misses_after_a,
            "first read of B (different NID) MUST miss ({misses_after_a} -> {misses_after_b})"
        );
        // Bytes for A are still correct (no cross-bleed).
        let mut buf_a_again = vec![0u8; payload_a.len()];
        fs.read_file(&i_a, 0, &mut buf_a_again).unwrap();
        assert_eq!(buf_a_again, payload_a);
    }

    #[test]
    fn pcluster_cache_with_capacity_builder() {
        // Builder-style override at open time is honoured, and stats
        // reflect the chosen capacity.
        let img = build_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev)
            .unwrap()
            .with_pcluster_cache_capacity(7);
        let (_, capacity, _, _) = fs.pcluster_cache_stats();
        assert_eq!(capacity, 7, "builder-style override must take effect");
    }

    #[test]
    fn pcluster_cache_plain_inode_does_not_populate() {
        // PLAIN (uncompressed) reads bypass the cache by design. Read
        // a FLAT_PLAIN file end-to-end and confirm the cache is still
        // empty afterwards (no hits, no misses, no entries).
        let img = build_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        let inode = fs.lookup_path("/hello.txt").unwrap();
        let mut buf = [0u8; 3];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"hi\n");
        let (entries, _, hits, misses) = fs.pcluster_cache_stats();
        assert_eq!(entries, 0);
        assert_eq!(hits, 0);
        assert_eq!(
            misses, 0,
            "PLAIN reads must not touch the decompression cache"
        );
    }

    // --- multi-device support ----------------------------------------
    //
    // These tests exercise `Filesystem::open_with_devices` and verify
    // that chunked reads with non-zero `device_id` route to the
    // matching extra-device handle. Compressed pclusters always carry
    // `device_id == 0` under the public spec, so multi-device routing
    // is a chunked-only feature for now.

    /// Build a synthetic 2-device EROFS image whose primary holds:
    /// - SB at byte 1024 with `extra_devices = 1`, `devt_slotoff = 16`
    ///   (so the device-table slot lives at byte 2048)
    /// - device-table slot 0 at byte 2048 with tag "extra1"
    /// - meta_blkaddr = 1: indexed chunked inode at NID 0 with two
    ///   chunks. Chunk 0 -> device 0 / blkaddr 4 ("AAAA…"). Chunk 1 ->
    ///   device 1 / blkaddr 0 ("BBBB…", served from the extra device).
    ///
    /// Returns `(primary_image, extra_device_image)`.
    fn build_two_device_image() -> (Vec<u8>, Vec<u8>) {
        const BS: usize = 4096;
        // Primary: 6 blocks (24 KiB).
        let mut primary = vec![0u8; BS * 6];

        // SB with extra_devices = 1 and devt_slotoff = 16 (byte 2048).
        let mut sb = synth_sb(12, 0, 1, 6);
        sb[0x56..0x58].copy_from_slice(&1u16.to_le_bytes()); // extra_devices
        sb[0x58..0x5A].copy_from_slice(&16u16.to_le_bytes()); // devt_slotoff
        primary[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb.len()]
            .copy_from_slice(&sb);

        // Device-table slot 0 at byte 2048: tag "extra1", blocks=1,
        // mapped_blkaddr=0.
        let slot_off = 16 * 128; // = 2048
        primary[slot_off..slot_off + 6].copy_from_slice(b"extra1");
        primary[slot_off + 64..slot_off + 68].copy_from_slice(&1u32.to_le_bytes());

        // Indexed-chunked inode at NID 0 (byte 4096). chunk_bits=0,
        // INDEXES set; size = 2 blocks; raw_format mirrors the
        // chunked.rs fixture (layout in bits 1..=3, flags in 4..=15).
        let mut inode = synth_compact(DataLayout::ChunkBased, 0x81A4, (BS * 2) as u32, 0);
        let raw_format: u16 = ((DataLayout::ChunkBased as u16) << 1)
            | (crate::chunked::EROFS_CHUNK_FORMAT_INDEXES << 4);
        inode[0x00..0x02].copy_from_slice(&raw_format.to_le_bytes());
        // i_u low 16 bits also carry chunk-format flags (chunked.rs
        // reads from i_u when non-zero — match its fallback path).
        inode[0x10..0x12]
            .copy_from_slice(&crate::chunked::EROFS_CHUNK_FORMAT_INDEXES.to_le_bytes());
        primary[BS..BS + 32].copy_from_slice(&inode);

        // Indexed chunkmap immediately after the 32-byte inode body.
        // Entry 0: device_id=0, blkaddr=4. Entry 1: device_id=1,
        // blkaddr=0 (i.e. block 0 of the extra device).
        let map_off = BS + 32;
        primary[map_off..map_off + 2].copy_from_slice(&0u16.to_le_bytes()); // advise
        primary[map_off + 2..map_off + 4].copy_from_slice(&0u16.to_le_bytes()); // device_id
        primary[map_off + 4..map_off + 8].copy_from_slice(&4u32.to_le_bytes()); // blkaddr
        primary[map_off + 8..map_off + 10].copy_from_slice(&0u16.to_le_bytes());
        primary[map_off + 10..map_off + 12].copy_from_slice(&1u16.to_le_bytes()); // device_id=1
        primary[map_off + 12..map_off + 16].copy_from_slice(&0u32.to_le_bytes()); // blkaddr=0

        // Chunk 0 data on the primary at block 4: filled with 'A'.
        for b in &mut primary[4 * BS..5 * BS] {
            *b = b'A';
        }

        // Extra device: 1 block, filled with 'B'.
        let extra = vec![b'B'; BS];

        (primary, extra)
    }

    #[test]
    fn open_with_devices_validates_extras_count() {
        // SB advertises 1 extra device but caller passes none -> error.
        let (primary, _extra) = build_two_device_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(primary)));
        match Filesystem::open_with_devices(dev, Vec::new()) {
            Err(Error::BadSuperblock(msg)) => {
                assert!(msg.contains("extra device"), "msg = {msg:?}");
            }
            Err(other) => panic!("expected BadSuperblock, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn open_with_devices_rejects_too_many_extras() {
        // Single-device image opened with a stray extra -> mismatch.
        let img = build_image();
        let primary: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let extra: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(vec![0u8; 4096])));
        match Filesystem::open_with_devices(primary, vec![extra]) {
            Err(Error::BadSuperblock(_)) => {}
            Err(other) => panic!("expected BadSuperblock, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn open_keeps_single_device_compat() {
        // The legacy single-device `open` continues to work after the
        // multi-device refactor (extras stays empty internally).
        let img = build_image();
        let dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(img)));
        let fs = Filesystem::open(dev).unwrap();
        assert!(fs.read_device_table().unwrap().is_empty());
    }

    #[test]
    fn read_device_table_surfaces_tags() {
        let (primary, extra) = build_two_device_image();
        let primary_dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(primary)));
        let extra_dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(extra)));
        let fs = Filesystem::open_with_devices(primary_dev, vec![extra_dev]).unwrap();
        let slots = fs.read_device_table().unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].tag_str(), "extra1");
        assert_eq!(slots[0].blocks, 1);
    }

    #[test]
    fn read_routes_to_correct_device() {
        // Chunk 0 -> primary ('A's), chunk 1 -> extra device ('B's).
        let (primary, extra) = build_two_device_image();
        let primary_dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(primary)));
        let extra_dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(extra)));
        let fs = Filesystem::open_with_devices(primary_dev, vec![extra_dev]).unwrap();
        let inode = fs.read_inode(0).unwrap();
        assert!(inode.is_regular_file());

        // Chunk 0: should be all 'A' from the primary.
        let mut buf = [0u8; 8];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"AAAAAAAA");

        // Chunk 1: must come from the EXTRA device.
        fs.read_file(&inode, 4096, &mut buf).unwrap();
        assert_eq!(&buf, b"BBBBBBBB");
    }

    #[test]
    fn read_block_helper_dispatches() {
        // Direct test of the dispatch helper: `device_id == 0` reads
        // from the primary (the byte we pre-populate); `device_id ==
        // 1` reads from the extra (different pre-populated byte).
        // Both with explicit byte offsets so the helper's offset-passthrough
        // is also exercised.
        let primary_bytes = [b'P'; 64];
        let extra_bytes = vec![b'E'; 64];
        // Primary needs a parseable SB so the FS can be opened.
        let (mut primary_img, _) = build_two_device_image();
        // Stamp a recognisable byte at offset 100 of the primary.
        primary_img[100] = b'P';

        let primary_dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(primary_img)));
        let extra_dev: Arc<dyn BlockRead> = Arc::new(MemDev(Mutex::new(extra_bytes.clone())));
        let fs = Filesystem::open_with_devices(primary_dev, vec![extra_dev]).unwrap();

        let mut buf = [0u8; 1];
        fs.read_block(0, 100, &mut buf).unwrap();
        assert_eq!(buf[0], b'P');
        fs.read_block(1, 0, &mut buf).unwrap();
        assert_eq!(buf[0], b'E');

        // device_id past the end of extras -> BadSuperblock.
        let err = fs.read_block(2, 0, &mut buf).unwrap_err();
        assert!(matches!(err, Error::BadSuperblock(_)));

        // The unused `primary_bytes` keeps the compiler happy that
        // we're matching on byte values, not addresses.
        assert_ne!(primary_bytes[0], extra_bytes[0]);
    }
}
