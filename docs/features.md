# Features

What this driver does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour updates its row
here, in the same pull request** (AGENTS.md). The reasoning behind each change
is in [CHANGELOG.md](../CHANGELOG.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request the changelog cites for it. Work merged after the last release
is **Unreleased (#N)** until the next one. **Tracking** names the issue for
anything not finished.

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

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Superblock, with its CRC32C verified at open | Supported | 0.1.1; checksum 0.2.0 (#52) | | `superblock_checksum.rs`, `oracle_compat.rs` |
| Compact (32-byte) and extended (64-byte) inodes | Supported | 0.1.1 | | `oracle_compat.rs`, `fixture_matrix.rs` |
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
| Library builder: symlinks, special files, hash-sorted directories, accurate `nlink` | Supported | 0.1.1 | | `oracle_writer.rs`, `round_trip.rs`, `kernel_readback.rs` |
| Byte-deterministic output | Supported | 0.1.1 | | |
| ZSTD compression on write | Not supported | | | |
| More than one codec in one image (`HEAD2` on write) | Not supported | | | |
| Multi-device images on write | Not supported: the reference `--blobdev` is broken in 1.9 | | | |
| `mkfs.erofs` tool: an uncompressed image of a directory, any block size | Supported | 0.1.1; named `mkfs.erofs` 0.3.0 | | `cli.rs`, `cli_mkfs_oracle.rs`, `cli_mkfs_kernel.rs` |
| `mkfs.erofs` tool: symlinks, special files and non-UTF-8 names | Partial: left out and named in the report's `skipped` list | 0.3.0 | | `cli.rs` |
| `mkfs.erofs --label` | Refused (exit 3) until the builder takes a volume name | 0.3.0 | | `cli.rs` |
| Changing an existing image | Not supported: read-only by format | | | |

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
| A Windows build of the tools | Upcoming | | #185 | |
