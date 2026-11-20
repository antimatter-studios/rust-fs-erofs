# The three API layers

Every rust-fs driver exposes the same three layers (rust-fs-core#453):

- **VolumeAPI**: open a source read-only, geometry, the superblock, the
  health of the volume as a whole and the features this driver supports
  for it.
- **PrimitiveAPI**: read-only, damage-tolerant access to the on-disk
  structures, plus the format-neutral salvage verbs.
- **FileAPI**: browse, stat, read and copy on a healthy volume.

This page maps what rust-fs-erofs exposes today onto those layers, names
what is missing, and says what each missing piece will do. It is the plan
for #311. Until rust-fs-core publishes the shared traits, the layers are
built here as ordinary types; when core's release is out they implement
its traits, and the public API this page lists as "today" stays one release
as a deprecated wrapper.

## What makes EROFS different

EROFS is read-only by format. There is no allocator, no free-space map, no
journal and no deletion, so a damaged image never holds a deleted file to
undelete, and the driver never invents allocation or deletion semantics.
"Salvage" here means reading what survives in a damaged image.

Four properties of the format shape the salvage design:

- **One superblock, no backup.** The 128-byte superblock at byte 1024 is
  the only copy. When it is unreadable, geometry (the block size above
  all) is a hypothesis the caller supplies or the driver infers from the
  metadata it finds, and is reported as one.
- **No inode table.** An inode is addressed by its NID: its byte offset is
  `meta_blkaddr * block_size + nid * 32`. The metadata area holds inodes,
  their inline xattrs and inline tails packed together, so finding inode
  candidates means scanning 32-byte slots for a plausible `i_format`, mode
  and size, not walking a table.
- **Data layouts carry their own maps.** FLAT_PLAIN is one contiguous run,
  FLAT_INLINE adds a tail inside the metadata area, CHUNK_BASED has a chunk
  index (block map or chunk indexes, possibly on extra devices), and the
  two compressed layouts have a cluster map of logical clusters over
  physical clusters (pclusters), with ztailpacking, fragments in the packed
  inode, and dedupe.
- **Compression has a unit.** A damaged pcluster loses only the logical
  range it decompresses to. Bytes that fail to decompress are reported
  missing, never replaced with zeros; only holes the format declares
  (a null chunk address, a zero-length tail) read as zeros.

## Today's public API, by layer

### VolumeAPI

| Today | Role | Gap |
|---|---|---|
| `Filesystem::open`, `open_with_cache`, `open_with_devices` | Opens a source as a `BlockRead`, so read-only by type already | Strict: a bad checksum, an unknown incompat bit, a COMPR_CFGS blob that does not parse or an extra-device count mismatch refuses the whole volume. There is no open that keeps going on a damaged superblock. |
| `superblock::read`, `Superblock::parse`, `Superblock` | The superblock and its fields | Returns an error instead of the parsed fields plus what is wrong with them. |
| `Superblock::block_size` and the `blocks`, `meta_blkaddr` and `xattr_blkaddr` fields | Geometry | No single geometry value with source length and a hypothesis flag. |
| `Superblock::verify_checksum`, `check_incompat_features` | Health checks | Each check refuses; there is no health report that lists every finding. |
| `Filesystem::read_device_table`, `superblock::DeviceSlot` | Extra devices of a multi-device image | A missing extra device refuses the open rather than marking its ranges unreadable. |
| `Filesystem::read_block` | Bounded read by device and offset | Kept; it becomes the VolumeAPI raw read. |
| `capi::fs_erofs_get_volume_info` | C ABI volume info | Unchanged. The C ABI is not part of this work. |

New:

- `RawVolume::open(source)` needs nothing valid: it gives source length and
  bounded reads, and the superblock as evidence when one parses.
- `Volume::open_for_salvage(source, hypothesis)` accepts a damaged
  superblock: it takes the fields that parse, the caller's block size where
  they do not, and records every check that failed.
- `Volume::health()` lists every finding (checksum, incompat bits, the
  COMPR_CFGS blob, the device table, the root inode) instead of stopping at
  the first.

### PrimitiveAPI: format-specific structures

| Structure | Today | Gap |
|---|---|---|
| Superblock | `Superblock::parse`, `superblock::read_compr_cfgs`, `superblock::read_device_table` | Evidence form: raw bytes, offset, parsed fields, findings. |
| Inodes, compact and extended | `Inode::parse`, `Inode::read`, `Inode::iloc`, `layout::InodeFormat`, `DataLayout` | `Inode::parse` refuses a bad format or layout. An evidence form keeps the raw 32 or 64 bytes, the fields that decode and the findings. |
| Xattrs, inline and shared, and the long-prefix dictionary | `parse_inline_xattrs`, `read_inline_xattrs`, `read_shared_xattrs`, `read_all_xattrs`, `read_xattr_prefix_dictionary`, `acl::parse` | One bad entry fails the whole set. The evidence form keeps the entries before the damage and reports where it starts. |
| Directory blocks | `dir::iter_block`, `dir::visit_block`, `Filesystem::read_dir` | One bad `nameoff` fails the block, and one bad block fails the directory. The evidence form keeps every dirent whose name and NID are in bounds, per block. |
| FLAT_PLAIN and FLAT_INLINE data | Inside `Filesystem::read_file` | A data-extent walk returns the block run and the inline tail as source extents. |
| CHUNK_BASED data | `chunked::chunk_info`, `lookup_chunk_blkaddr`, `ChunkInfo` | A walk over every chunk: device, block address, null (a declared hole), or unreadable index. |
| Compressed data (LZ4, LZMA, DEFLATE, ZSTD) | `ZMap::open`, `read_lcluster`, `map`, `pcluster_extent`, `fragment_range`, `decompress*` | A walk over every logical cluster to its pcluster, algorithm and logical range; a decode of one pcluster that reports failure for that range only. |
| Packed inode, fragments, dedupe | Inside `ZMap` and `Filesystem` | Exposed as extents that name the packed inode as their source. |

### PrimitiveAPI: the salvage verbs

The verbs are rust-fs-exfat's `SalvageAdapter` (its `docs/convention.md`
section 4), moving to rust-fs-core. For EROFS they mean:

| Verb | EROFS meaning |
|---|---|
| Find candidates | Scan the metadata area (and, without a superblock, the whole source in 32-byte steps) for inodes that decode, and directory blocks whose dirents are in bounds. Each candidate carries its NID, source offset, the decoded fields and the evidence for and against. |
| Walk extents | For a candidate inode, the source byte extents of its data in file order: the plain run, the inline tail, chunks, or pclusters with their algorithm and logical range. The walk ends `Complete`, `Broken` (an index or cluster map that does not decode), `OutOfRange` or `Unreadable`, keeping what it has. |
| Rebuild the tree | Parent claims from every directory block found, plus each directory's `..` entry. An inode no directory names is an orphan; one named by two directories is a conflict, kept, not resolved. Hard links are the one legitimate case of several names, and are reported as such when `nlink` agrees. |
| Report damage | Superblock findings, inodes that do not decode, directory blocks that do not, cluster maps and chunk indexes that point outside the source, pclusters that fail to decompress, and missing extra devices. Each names its source range and structure. |
| Read by extent | Raw source bytes for an extent. For a compressed extent, the decompressed bytes of that pcluster, or a failure for that range only. |
| Materialize from a plan | Write a file from its extents to a caller's `io::Write`, recording the source of every byte; ranges that could not be read or decompressed are reported missing, not filled. |
| Capabilities | Present: every verb. Absent by format, and named as such: deleted candidates and deleted extents, because EROFS has no deletion. |

### FileAPI

| Today | Role |
|---|---|
| `Filesystem::root_inode`, `read_inode` | Stat by NID |
| `Filesystem::read_dir`, `lookup`, `lookup_path`, `lookup_path_bytes`, `resolve_path` | Browse and path lookup |
| `Filesystem::read_file` | Ranged reads |
| `Filesystem::read_symlink_target` | Symlinks |
| `Filesystem::xattrs`, `acl::parse` | Extended attributes and POSIX ACLs |
| `Inode::file_type`, `rdev`, `is_*` | File type and device numbers |

These already do what FileAPI asks of a healthy volume. The work is to put
them behind the shared trait and to add a streaming copy. They stay strict:
on a damaged volume FileAPI refuses, and salvage goes through PrimitiveAPI.

### Not in any layer

`mkfs` builds images, and the command-line tools and the C ABI wrap the
reader. None of them changes in this work.

## Read-only by type

Every call that touches the source takes `BlockRead`, which cannot write,
and nothing here takes `BlockDevice`. The driver has no write path into an
existing image to begin with, so no runtime flag is needed or offered.
Materialization writes only to a caller's `io::Write`.

## How it is tested

- **Healthy images**: made by `mkfs.erofs` in the fs-linux-test-harness
  guest, mounted by the Linux kernel there, and compared file by file
  (listing, sizes, bytes) with FileAPI and with the salvage verbs.
- **Damaged images**: the same images with the superblock overwritten, a
  metadata block overwritten, a directory block overwritten, a compressed
  pcluster overwritten, the image truncated, and an extra device missing.
  The kernel and `fsck.erofs` say which files survive; the salvage verbs
  must recover those files' bytes and report damage for the rest, never
  invent bytes for it.

## Order of work

1. Pin rust-fs-core by version (#312).
2. This map.
3. Damaged-image fixtures and oracle tests that record what the kernel and
   `fsck.erofs` read in each.
4. Damage-tolerant primitives, one structure per pull request.
5. The salvage verbs over them.
6. When rust-fs-core publishes the traits: VolumeAPI, PrimitiveAPI and
   FileAPI on them, with today's API kept one release as a deprecated
   wrapper.
