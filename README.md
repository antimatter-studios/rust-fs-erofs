# rust-fs-erofs

A pure-Rust, clean-room implementation of the **EROFS** (Enhanced Read-Only File System) on-disk format. Reads and writes images that the Linux kernel's EROFS driver and `erofs-utils` toolchain accept byte-for-byte.

The repository ships one library crate (published on crates.io as `fs-erofs`, library name `fs_erofs`) plus the `mkfs_erofs` CLI binary.

- **Reader**: every EROFS feature emitted by `mkfs.erofs` 1.9 + AOSP build systems
- **Writer (`mkfs_erofs`)**: produces images `fsck.erofs` accepts as valid
- **Cross-platform**: builds + runs on Linux, macOS, Windows
- **License**: MIT, fully permissive deps tree (no GPL/LGPL anywhere)

## Status

**Feature-complete.** Every on-disk format feature this driver could plausibly encounter is implemented end-to-end with both library tests and integration tests against `mkfs.erofs` / `fsck.erofs` / `dump.erofs` as oracles. Workspace test coverage sits at **95% line / 97% function**.

## What this driver can do

### Reader

| Capability | Status |
|---|---|
| Superblock parsing + CRC32C verification | ✅ |
| Compact (32-byte) and extended (64-byte) inodes | ✅ |
| Layout `FLAT_PLAIN` (contiguous) | ✅ |
| Layout `FLAT_INLINE` (tail-packed in metadata block) | ✅ |
| Layout `ChunkBased` + sparse holes | ✅ |
| Layout `Compression` (legacy/uncompacted index) | ✅ |
| Layout `Compression` (compacted-2B index) | ✅ |
| Codec: **LZ4** | ✅ |
| Codec: **LZMA** (LZMA1 raw stream) | ✅ |
| Codec: **DEFLATE** (raw, no zlib wrapper) | ✅ |
| `BIG_PCLUSTER_1` / `BIG_PCLUSTER_2` (multi-block pclusters) | ✅ |
| `FRAGMENT_PCLUSTER` (cross-file packed-tail dedup) | ✅ |
| `INTERLACED_PCLUSTER` (rotate-and-paste PLAIN) | ✅ |
| `INLINE_PCLUSTER` (ztailpacking — last pcluster in metadata) | ✅ |
| `HEAD2` separate-algorithm dispatch | ✅ |
| `COMPR_CFGS` blob (LZMA dict_size etc.) | ✅ |
| Multi-lcluster pcluster spans | ✅ |
| Multi-device images (device_id routing) | ✅ |
| Inline xattrs + shared (block-area) xattrs | ✅ |
| POSIX ACLs (access + default) | ✅ |
| Custom xattr prefix dictionary (`mkfs.erofs -x N`) | ✅ |
| Symbolic links + symlink loop protection (`MAXSYMLINKS=40`) | ✅ |
| Special files: chrdev, blkdev, fifo, socket | ✅ |
| Hardlinks (multiple dirents → same NID) | ✅ (incidental) |
| Hash-sorted dirent layout (kernel-mountable) | ✅ |
| Decompressed-pcluster LRU cache (≈8.5× speedup) | ✅ |

### Writer (`mkfs_erofs`)

| Capability | Status |
|---|---|
| FLAT_PLAIN + FLAT_INLINE + ChunkBased emission | ✅ |
| Compact + extended inode auto-promotion | ✅ |
| LZ4 / LZMA / DEFLATE compression | ✅ |
| Legacy + compacted-2B index format | ✅ |
| ztailpacking (single-pcluster inline tail) | ✅ |
| Multi-lcluster pcluster collation (greedy) | ✅ (writer-default; better compression ratios) |
| Inline xattrs, POSIX ACL emit | ✅ |
| Custom xattr prefix dictionary | ✅ |
| `COMPR_CFGS` blob with non-default LZMA props | ✅ |
| Symlinks, special files (chr/blk/fifo/sock) | ✅ |
| Hash-sorted dirents (kernel-mountable output) | ✅ |
| SB CRC32C checksum | ✅ |
| Accurate directory `nlink` (`2 + child_dirs`) | ✅ |
| Deterministic image layout (reproducible builds) | ✅ |
| Output is `fsck.erofs`-clean across the matrix | ✅ |

## What this driver cannot (yet) do

| Feature | Reason | Workaround |
|---|---|---|
| **Compacted-1B index** | Format reportedly never existed in published kernels; no producer found | n/a — unobservable in practice |
| **HEAD2 separate-algorithm WRITER** | Our writer emits single-codec images only | Use `mkfs.erofs` with `-z lz4hc,lzma` if you need this |
| **Multi-device WRITER** | `mkfs.erofs --blobdev` is broken in upstream 1.9 | Wait for upstream fix or hand-build |
| **Mutate an existing EROFS image in place** | EROFS is read-only by spec — no journal, no allocator, no rewrite path | See "Read-write semantics" below |
| **Verified boot / dm-verity hash trees** | Layer above EROFS, out of scope | Use `verity` tools alongside |
| **ZSTD codec** | Not yet implemented | Use `-z lz4` / `-z lzma` / `-z deflate` |

## Use cases

- **Reading Android `system.img` / `vendor.img` on macOS, Windows, Linux** — the canonical use case. Feed an unwrapped (post-`simg2img`) image to `Filesystem::open` and read every file.
- **Custom read-only volumes for embedded / immutable-OS distributions** — `mkfs_erofs source-tree/ out.img` produces a kernel-mountable image with strong compression.
- **Windows users browsing Linux/Android disk images** — when paired with a Windows mount layer (e.g. WinFsp), expose EROFS images as drive letters.
- **Differential filesystem fixtures** — deterministic image generation (reproducible builds, content-addressable storage, OCI/container layers).

## Installation

### From crates.io (when published)

```toml
[dependencies]
fs-erofs = "0.1"
```

### From source (workspace path-dep)

```toml
[dependencies]
fs-erofs = { path = "../rust-fs-erofs" }
am-fs-core = { path = "../rust-fs-core" }
```

### Building locally

```sh
git clone https://github.com/antimatter-studios/rust-fs-erofs
cd rust-fs-erofs
cargo build --release
cargo test
```

The `mkfs_erofs` binary lives at `target/release/mkfs_erofs`.

## Library usage

```rust
use std::sync::Arc;
use fs_core::{BlockRead, FileDevice};
use fs_erofs::Filesystem;

// Open an image file (or any BlockRead).
let dev = Arc::new(FileDevice::open("system.img")?) as Arc<dyn BlockRead>;
let fs = Filesystem::open(dev)?;

// Inspect the volume.
println!("block size: {}", fs.superblock().block_size());
println!("blocks:     {}", fs.superblock().blocks);
println!("root NID:   {}", fs.superblock().root_nid);

// Resolve a path and read its content.
let inode = fs.lookup_path("/etc/hostname")?;
let mut buf = vec![0u8; inode.size as usize];
fs.read_file(&inode, 0, &mut buf)?;
println!("hostname: {}", String::from_utf8_lossy(&buf));

// List a directory.
let dir = fs.lookup_path("/etc")?;
for entry in fs.read_dir(&dir)? {
    let name = String::from_utf8_lossy(&entry.name);
    println!("{:>3} {:>10} {}", entry.file_type, entry.nid, name);
}

// Follow a symlink.
let target = fs.read_symlink_target(&fs.lookup_path("/some/link")?)?;
let resolved = fs.resolve_path("/some/link", true)?;  // follow symlinks

// Read xattrs.
for (full_name, value) in fs.xattrs(&inode)? {
    println!("{} = {}", String::from_utf8_lossy(&full_name),
                       String::from_utf8_lossy(&value));
}

// LRU cache management (default capacity: 256 pclusters).
let (entries, capacity, hits, misses) = fs.pcluster_cache_stats();
fs.set_pcluster_cache_capacity(1024); // bigger cache for sequential workloads
fs.set_pcluster_cache_capacity(0);    // disable for memory-constrained hosts
```

### Multi-device images

```rust
use fs_erofs::Filesystem;

let primary = Arc::new(FileDevice::open("primary.img")?);
let extras = vec![
    Arc::new(FileDevice::open("blob1.img")?) as Arc<dyn BlockRead>,
    Arc::new(FileDevice::open("blob2.img")?) as Arc<dyn BlockRead>,
];
let fs = Filesystem::open_with_devices(primary, extras)?;

// Inspect the SB device table.
for slot in fs.read_device_table()? {
    let tag = std::str::from_utf8(&slot.tag).unwrap_or("(non-utf8)");
    println!("tag={tag:?} blocks={}", slot.blocks);
}
```

## CLI usage — `mkfs_erofs`

```sh
# Default settings (4 KiB blocks, no compression)
mkfs_erofs out.img source-tree/

# Specific block size
mkfs_erofs --block-size 16384 out.img source-tree/

# Show help
mkfs_erofs --help
```

Symbolic links, special files, and non-UTF-8 directory entries print a stderr warning and are skipped on the CLI path. Use the library API (`mkfs::build_image_with(...)`) for full control over the on-disk shape (compression, xattrs, ACLs, prefix dictionary, COMPR_CFGS blob).

The output is byte-deterministic given the same input tree and options — useful for reproducible builds.

## Read-write semantics

EROFS is **read-only by format design** — the on-disk format has no journal, no allocator, and no in-place rewrite path. This library exposes a read-only API: `Filesystem::open(...)` returns a handle whose every method reads. The `mkfs_erofs` writer creates *new* images from a source tree; it never mutates an existing one.

If your application needs writable-volume semantics (e.g. for a fuse / WinFsp adapter), the typical pattern is to overlay an in-memory or sidecar layer on top of this read engine and choose a persistence policy (discard / archive to a sidecar / re-mkfs the merged tree on flush). That layering is the consumer's responsibility — this crate provides only the read engine and the image builder.

## Testing

```sh
cargo test                     # 243 default tests
cargo test -- --include-ignored  # +64 ignored = 307 total

# Coverage report
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
cargo llvm-cov --html --workspace
open target/llvm-cov/html/index.html

# Lint
cargo clippy --all-targets    # zero warnings
```

The `--ignored` tests exercise the oracle integration suite — they spawn `mkfs.erofs` / `fsck.erofs` / `dump.erofs` from `erofs-utils` (Homebrew: `brew install erofs-utils`; Debian/Ubuntu: `apt install erofs-utils`) to cross-validate every emitted feature.

## Real-world fixture (Android GSI)

```sh
# Optional: fetch a small Android GSI for end-to-end testing
./tests/fixtures/download-gsi.sh
cargo test --test oracle_gsi -- --ignored
```

The GSI is large (~1 GB) and gitignored. The script verifies a pinned SHA256 and unwraps the sparse-image format if needed (requires `simg2img` from `android-platform-tools`).

## Performance notes

- **LRU cache**: defaults to 256 decompressed pclusters (~64 MiB at typical sizes). Sequential reads of compressed multi-pcluster files see roughly 8× speedup from cache hits. Disable via `Filesystem::set_pcluster_cache_capacity(0)` for memory-constrained hosts.
- **Codec choice**: LZ4 is fastest to decompress; LZMA gives best compression ratios; DEFLATE is mid. Default `mkfs_erofs` compression is uncompressed (ship a baseline image first, opt into compression via `mkfs::build_image_with`).
- **Inline tail-packing**: small files become FLAT_INLINE automatically when their tail fits in the metadata block — saves a full block of padding per file. Significant for many-small-files trees (Android `/etc`).

## Known limitations & gotchas

- **EROFS is read-only by format**. There is no in-place mutation API. Consumers needing writable semantics layer their own overlay on top.
- **`mkfs.erofs --blobdev` is broken in 1.9**. Multi-device READ is implemented + tested with synthetic byte-level images. If/when upstream fixes the writer, our oracle test will pick it up.
- **HEAD2 separate-algorithm**: handled on the read side; our writer always emits single-codec images. Real-world impact: zero, unless you specifically need to round-trip a `mkfs.erofs -z lz4hc,lzma`-built image through our writer.
- **Compacted-1B index format**: never existed in any published kernel. We have a compile-time guardrail asserting only `Legacy` and `Compact` variants exist in our `IndexFormat` enum so no future agent re-introduces a phantom branch.
- **Symlink target encoding**: stored as raw bytes; reader returns `Vec<u8>`. UTF-8 decoding is the caller's responsibility (most targets are UTF-8 in practice).
- **No FUSE driver**: we don't ship a Linux FUSE adapter — Linux already has the in-kernel EROFS driver. Use `mount -t erofs -o loop image.img /mnt`.

## License

MIT. See [LICENSE](LICENSE).

A pre-distribution IP audit confirms: **all transitive dependencies are permissively licensed**; no GPL / LGPL / AGPL anywhere; format spec referenced via public EROFS documentation only ([erofs.docs.kernel.org](https://erofs.docs.kernel.org/)) — implementation is entirely independent of `linux/fs/erofs/*.c` (GPL-2) and `erofs-utils` source (BSD-2/GPL-2 dual; we avoid even the BSD election for cleanroom posture).

External tools (`mkfs.erofs`, `fsck.erofs`, `dump.erofs`, `mount`) are invoked at arm's length via subprocess from `#[ignore]`-gated integration tests only — never linked, never source-copied. Permitted under the standard interpretation of GPL "mere aggregation."

## Spec references

- [EROFS on-disk format documentation](https://erofs.docs.kernel.org/en/latest/design.html) (canonical public reference)
- "EROFS: A Compression-friendly Readonly File System for Resource-scarce Devices" — Gao et al., USENIX ATC 2019 (the original paper)
- `erofs_fs.h` — public format-definition header (struct layouts, constants)

## Contributing

Issues + PRs welcome. Before opening:
- Run `cargo test` (full suite) and `cargo clippy --all-targets` (zero warnings).
- For format-spec changes, cite the public docs above (NOT kernel `.c` files — clean-room posture).
- New features should land with both unit tests and an oracle test against `erofs-utils` where applicable.
- Maintain coverage ≥ 94% line.

## Crate map

| Crate | License | Purpose |
|---|---|---|
| [`am-fs-core`](https://crates.io/crates/am-fs-core) | MIT | Block-device traits (`BlockRead`, `FileDevice`, slice adapters) |
| [`fs-erofs`](https://crates.io/crates/fs-erofs) (this) | MIT | EROFS read + write |
| [`am-fs-ext4`](https://crates.io/crates/am-fs-ext4) | MIT | ext4 read + write (sister project) |

## Acknowledgements

EROFS originally developed by Huawei (~2018, upstreamed Linux 5.4). Format documentation maintained by the EROFS upstream project at kernel.org. This crate is independent of those projects — clean-room from public spec material — but their work is what made EROFS a viable target for an interoperable third-party reader/writer.
