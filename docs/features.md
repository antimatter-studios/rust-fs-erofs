# Features

What this driver does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour adds its row to
[`features.d/`](../features.d/README.md), in the same pull request**, and the
next release writes it here (AGENTS.md). The reasoning behind each change is
in [CHANGELOG.md](../CHANGELOG.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request the changelog cites for it. Work merged after the last release
is **Unreleased (#N)** in its `features.d/` fragment until the next one.
**Tracking** names the issue for anything not finished.

EROFS is read-only by format: there is no journal, no allocator and no way to
rewrite an image in place. This crate reads images and builds new ones; it
never changes an existing one.

States:

- **Supported**: works, and is checked against erofs-utils or the Linux
  kernel in the harness VM.
- **Experimental**: works in every test, but is new.
- **Partial**: works for part of the case, and the row says which part.
- **Refused**: recognised and refused by name, rather than misread.
- **Not supported**: neither read nor refused by name.
- **Upcoming**: an open issue with a plan.
- **Unobservable**: no tool makes an image that shows it, so it cannot be
  tested against anything.
- **Removed**: worked in an earlier release and no longer does. **Since**
  is the release it went away in, and the row says why and which releases
  had it.

**A row is never deleted.** A feature that goes away keeps its row, in the
**Removed** state, so the page still answers why it is missing. A row that
vanished would look exactly like a feature that was never there, and
would hide a removal that was really a regression to fix.

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Superblock, with its CRC32C verified at open | Supported | 0.1.1; checksum 0.2.0 (#52) | | `superblock_checksum.rs`, `oracle_compat.rs` |
| Compact (32-byte) and extended (64-byte) inodes | Supported; a compact inode's mtime is the build time | 0.1.1; mtime 0.6.0 (#252) | | `oracle_compat.rs`, `fixture_matrix.rs`, `mkfs::tests::mtime_is_build_time_makes_every_inode_compact_with_that_mtime` |
| Layouts: `FLAT_PLAIN`, `FLAT_INLINE`, chunk-based with sparse holes | Supported | 0.1.1 | | `oracle_compat.rs`, `fixture_matrix.rs` |
| Compressed layout, legacy and compacted-2B indexes | Supported | 0.1.1 | | `oracle_compat.rs`, `fixture_matrix.rs` |
| Compacted-1B index | Unobservable: no published producer | | | |
| Codecs: LZ4, LZMA, DEFLATE | Supported | 0.1.1 | | `oracle_compat.rs`, `kernel_readback.rs` |
| Codec: ZSTD | Supported | 0.2.0 | | `zstd_image.rs` |
| Big pclusters, fragments, interlaced pclusters, multi-lcluster spans | Supported | 0.1.1 | | `oracle_compat.rs`, `fixture_matrix.rs` |
| Inline pclusters (ztailpacking) | Supported | 0.1.1 | | `oracle_compat.rs`, `fixture_matrix.rs` |
| The file in an `-Eall-fragments,ztailpacking` inline tail | Refused: erofs-utils 1.9.1 writes it in a shape the format cannot read; the rest of the image reads | 0.3.0 | | `fixture_matrix.rs` |
| Deduplicated images (`-Ededupe`, `-Efragments,dedupe`) | Supported | 0.3.0 (#125) | | `fixture_matrix.rs`, `oracle_compat.rs` |
| `-Ededupe` without fragments, from erofs-utils 1.9.1 | Partial: lookups of the inodes that mkfs never wrote fail; nothing reads as another file's bytes | 0.3.0 (#125) | | `fixture_matrix.rs` |
| `HEAD2` second-algorithm dispatch, `COMPR_CFGS` blob | Supported | 0.1.1 | | `oracle_compat.rs` |
| Zero padding decided by the superblock's bit | Supported | 0.2.0 | | `zero_padding_wiring.rs` |
| Multi-device images (device-id routing) | Supported | 0.1.1 | | `oracle_compat.rs` |
| Extended attributes, inline and shared; the custom prefix dictionary | Supported | 0.1.1 | | `oracle_compat.rs`, `fixture_matrix.rs` |
| POSIX ACLs, access and default | Supported | 0.1.1 | | `acl_oracle.rs` |
| Symlinks, with loop protection at 40 hops | Supported | 0.1.1 | | `capi_read.rs`, `src/fs.rs` unit tests |
| Special files: character and block devices, FIFOs, sockets | Supported | 0.1.1 | | `round_trip.rs`, `capi_dir.rs` |
| Hard links: link counts and inode identity | Supported | 0.1.1 | | `hardlink_oracle.rs` |
| Hash-sorted directories; one name resolved without decoding the directory | Supported | 0.1.1; lookup cost 0.2.0 | | `lookup_allocations.rs` |
| A nonzero `dirblkbits` | Refused | 0.2.0 (#58) | | `dirblkbits.rs` |
| 48-bit block addressing (`-E48bit`) and the metabox (`-m`) | Refused by name | 0.2.0 | | `incompat_gate.rs`, `feature_bits_oracle.rs` |
| Any other unknown incompatible feature bit | Refused | 0.2.0 | | `incompat_gate.rs` |
| A damaged image | Refused, as the kernel refuses it | 0.2.0 | | `kernel_readback.rs`, `oracle_verdicts.rs` |
| Decompressed-pcluster cache; file data kept out of the metadata cache | Supported | 0.1.1; separate caches 0.2.0 (#69) | | `data_bypasses_metadata_cache.rs`, `read_path_cost.rs` |
| A real Android `system.img` | Supported | 0.3.1 | | `oracle_android.rs` |
| Fuzzed decoders | Supported | 0.2.0 | | `fuzz_decoders.rs` |
| Verified boot / dm-verity hash trees | Not supported: a layer above EROFS | | | |

## Building an image

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Library builder (`mkfs::build_image_with`): `FLAT_PLAIN`, `FLAT_INLINE`, chunk-based; compact and extended inodes | Supported | 0.1.1 | | `oracle_writer.rs`, `round_trip.rs` |
| Library builder compression: LZ4, LZMA, DEFLATE, legacy and compacted-2B indexes, ztailpacking, multi-lcluster pclusters | Supported | 0.1.1 | | `oracle_writer.rs`, `round_trip.rs` |
| Library builder: inline xattrs, POSIX ACLs, the prefix dictionary, `COMPR_CFGS` | Supported | 0.1.1 | | `oracle_writer.rs` |
| Library builder: symlinks, special files, hash-sorted directories, accurate `nlink` | Supported; "." and ".." sorted among the names since 0.6.0 (#258) | 0.1.1 | | `oracle_writer.rs`, `round_trip.rs`, `kernel_readback.rs` |
| Byte-deterministic output | Supported | 0.1.1 | | `lz4hc_writer.rs` (`the_same_tree_compresses_to_the_same_bytes`, `any_thread_count_builds_the_same_bytes`) |
| ZSTD compression on write | Not supported | | | |
| More than one codec in one image (`HEAD2` on write) | Not supported | | | |
| Multi-device images on write | Not supported: the reference `--blobdev` is broken in 1.9 | | | |
| `mkfs.erofs` tool: an uncompressed image of a directory, any block size | Supported | 0.1.1; named `mkfs.erofs` 0.3.0 | | `cli.rs`, `cli_mkfs_oracle.rs`, `cli_mkfs_kernel.rs` |
| `mkfs.erofs` tool: symlinks, special files and non-UTF-8 names | Partial: left out and named in the report's `skipped` list | 0.3.0 | | `cli.rs` |
| `mkfs.erofs --label` | Refused (exit 3) until the builder takes a volume name | 0.3.0 | | `cli.rs` |
| Changing an existing image | Not supported: read-only by format | | | |
| Library builder: an LZ4HC image within 2% of `mkfs.erofs -zlz4hc,12 -C 32768`, inodes with inline tails packed into the room earlier metadata blocks left | Supported | 0.6.0 (#259) | | `oracle_size_parity.rs`, `inline_tails.rs` |
| Library builder: file contents that borrow (`Content<'a>`, `Node<'a>`), not `'static` | Supported | 0.6.0 (#263) | | `streaming_writer.rs`, `oracle_writer.rs` |
| Library builder: an xattr from its full name (`XattrSpec::from_full_name`), unknown prefixes refused | Supported | 0.6.0 (#263) | | `mkfs::tests::a_full_name_converts_back_to_the_index_and_name_it_resolved_from` |
| Library builder: hard links (`Node::HardLink`), each inode's `nlink` counting its names | Supported | 0.6.0 (#253) | | `mkfs::tests::hard_links_name_one_inode_and_count_every_name`, `hardlink_oracle.rs` |
| Library builder: inline tails (`FLAT_INLINE`) for directories, symlinks and uncompressed files of any size | Supported | 0.6.0 (#257) | | `inline_tails.rs`, `oracle_writer.rs` |
| Library builder: LZ4HC compression into big pclusters (`BuildOptions::compression`, as `-zlz4hc,12 -C 32768`), stored blocks and stored files where compression saves nothing | Supported | 0.6.0 (#256) | | `lz4hc_writer.rs`, `streaming_memory.rs` |
| Library builder: big pclusters in the compacted index | Not supported: big pclusters use the legacy index | | | |
| Library builder: parallel compression (`BuildOptions::threads`), the same bytes on any number of threads | Supported | 0.6.0 (#262) | | `lz4hc_writer.rs`, `streaming_memory.rs` |
| Library builder: superblock UUID and build time; every mtime the build time (`-T`), with compact inodes | Supported | 0.6.0 (#252) | | `mkfs::tests::the_superblock_carries_the_requested_uuid_and_build_time`, `oracle_writer.rs` |
| Library builder: shared xattrs for values two or more inodes carry (`share_xattrs`) | Supported | 0.6.0 (#254) | | `mkfs::tests::repeated_xattrs_are_shared_and_every_inode_reads_its_own_set`, `oracle_writer.rs` |
| Library builder streaming: content from readers (`Node::StreamedFile`), the image to a `Write + Seek` sink (`build_image_to`), memory bounded by metadata | Supported | 0.6.0 (#255) | | `streaming_writer.rs`, `streaming_memory.rs`, `oracle_writer.rs` |

## Interfaces

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| C ABI: mount (path, callbacks, fs_core device), volume info, stat, directory iterator, read, readlink, xattrs | Supported | 0.1.1 | | `capi_basic.rs`, `capi_dir.rs`, `capi_read.rs` |
| C ABI readlink returns the target's length | Supported | 0.2.0 (#138) | | `capi_read.rs` |
| C ABI paths as bytes, not UTF-8 | Supported | 0.3.0 (#148) | | `capi_read.rs` |
| `fs.erofs` `ls`, `read`, `get`/`info` (`--features cli`) | Supported | 0.3.0 | | `cli_fs.rs`, `cli_fs_oracle.rs`, `cli_fs_kernel.rs` |
| `fs.erofs` `write`, `mkdir`, `set`, `resize` | Refused: "EROFS is read-only" (exit 3) | 0.3.0 | | `tests/cli/test-fs.sh` |
| `rust-fs-erofs doctor`, man pages, shell completions | Supported | 0.3.0 | | `cli_dispatch.rs`, `cli_docs.rs` |
| Release tarballs: darwin-arm64, linux-x86_64 | Supported | 0.3.0 | | `release_attestation.rs` |
| Release zips for Windows: windows-x86_64, windows-arm64, each dotted name a copy of `rust-fs-erofs.exe` | Supported | 0.4.1 (#185) | | `release_attestation.rs` |
| The tools tested on Windows, from the release zip (`cli-windows` job) | Supported | 0.4.1 (#185) | | `tests/cli/test-*.sh` |
| The tools published to winget | Upcoming: the release job submits each version, but the package is not registered with winget yet | | #281 | |
| `fs.erofs` `export PATH DEST` | Supported: bounded single-file transfer, SHA-256, stdout, replacement refusal and atomic publication; directories, symlinks and special files refused | 0.5.0 (#226) | #224 | `cli_export_kernel.rs`, `cli_docs.rs` |
| `fs.erofs` recursive export | Supported: shared TreeSource traversal, per-item reports, depth bound, directory merging and existing-file refusal; symlinks require `--preserve=links` and special files are skipped | 0.5.0 (#237) | #232 | `cli_export_tree_kernel.rs` |
| `fs.erofs` recursive export policy regression coverage | Supported: exact depth bounds, directory merging, replacement refusal, image offsets and unchanged single-file commands, against the kernel | 0.5.0 (#242) | | `cli_export_tree_kernel.rs` |
| `fs.erofs` `import SOURCE PATH [--replace]` | Refused: "EROFS is read-only" (exit 3), before opening the image or source file or reading stdin | 0.5.0 (#228) | #215 | `cli_import.rs`, `cli_docs.rs` |
| `fs.erofs ls --recursive`: byte-sorted depth-first paths and depths, finite bounds, partial reports and directory-loop safety (`--features cli`) | Supported | 0.5.0 (#219) | #211 | `cli_ls_recursive_kernel.rs`, `tests/cli/test-ls-recursive.sh` |
| `fs.erofs` `rm`, `rmdir`, `mv`, `ln` (including `-s`), `truncate`, `mkdir --parents`, `write --at` | Refused: "EROFS is read-only" (exit 3), before opening the image or reading stdin | 0.5.0 (#221) | #212 | `tests/cli/test-fs.sh`, `cli_docs.rs` |
| `fs.erofs` recursive export metadata preservation | Supported: `--preserve=links,times` recreates hard links and opaque symlinks and sets recorded modification times; access times are unavailable and symlink times are not set | 0.5.0 (#241) | #239, #236 | `cli_export_tree_kernel.rs`, `cli_docs.rs` |
| `fs.erofs stat`: one path's identity, type, owner, links, sizes, effective mtime, symlink target, device numbers and inode metadata; unrecorded times are null (`--features cli`) | Supported | 0.5.0 (#217) | | `cli_stat_kernel.rs`, `tests/cli/test-stat.sh` |
| `fs.erofs` `xattr PATH [NAME]` | Supported: byte-sorted JSON/text names and sizes, raw values, `-o`, missing-attribute refusals and inline/shared/prefix-dictionary storage | 0.5.0 (#222) | #213 | `tests/cli/test-xattr.sh`, `cli_xattr_kernel.rs`, `cli_docs.rs` |
| `fs.erofs` reading verbs (`ls`, `ls -R`, `stat`, `read`, `export`, `export --recursive`) on a missing path: exit 1 with `"kind": "not_found"` in the JSON error, as the CLI contract has it | Supported | 0.5.1 (#251) | #251 | `cli::erofs::fs::tests::a_missing_path_is_the_not_found_kind`, `tests/cli/test-fs.sh`, `test-stat.sh`, `test-ls-recursive.sh` |
| `fs.erofs ls --recursive` shared arguments, help, defaults and bound-reached errors (`--features cli`) | Supported: rust-fs-core supplies the CLI contract; the byte-sorted depth-first walk stays in the driver | 0.6.0 (#273) | antimatter-studios/rust-fs-core#406 | `cli_listing.rs`, `cli_ls_recursive_kernel.rs`, `tests/cli/test-ls-recursive.sh` |
| `chore staticlib` with cargo's target directory moved | Supported | 0.6.0 (#283) | | `test-staticlib-target-dir.sh` |
