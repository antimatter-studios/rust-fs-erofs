# Changelog

Notable changes to `am-fs-erofs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

### Added

- **A `mkfs.erofs` geometry matrix, as fixtures.** `chore fixtures`
  builds eleven images in the guest from one generated source tree —
  uncompressed, each of LZ4, LZ4HC, LZMA, DEFLATE and ZSTD, three block
  sizes, a 64 KiB pcluster, an inlined tail and a chunked inode — and
  `tests/fixture_matrix.rs` reads every file in every one of them back.
  They are ordinary files afterwards, so the aarch64 CI job, which has
  no KVM and therefore no VM, reads genuine `mkfs.erofs` output for the
  first time. The expectation is regenerated from the same rule the
  guest generated the tree from, not recorded from what the reader
  returned.

- **The kernel must refuse a damaged image.** `tests/kernel_readback.rs`
  mounts what the writer emitted and compares every file by type, mode,
  size, symlink target and SHA-256 — and then overwrites the superblock
  magic and requires the mount to fail. Without the second half, "the
  kernel mounted it" is a claim a driver emitting an unconditionally
  acceptable superblock would also satisfy.

- **The parsers are fuzzed, on two tiers.** EROFS is read-only and mounted
  from sources the reader did not produce — a container image, an
  appliance, an OTA payload — and nothing here had a fuzz target.
  `fuzz/` holds five `cargo-fuzz` targets and runs nightly on a bounded
  budget; `tests/fuzz_decoders.rs` is the gate, replaying and mutating the
  same corpus deterministically on the stable toolchain in under half a
  second.

  The corpus is five images `mkfs.erofs` wrote and `fsck.erofs` accepted —
  one per compression algorithm, plus a chunk-based one, since the
  algorithm id and the datalayout each select a different decode path —
  rebuilt by `scripts/make-fuzz-corpus.sh`. The `image` target opens and
  walks a mutated one, which is the only way `zmap`, `chunked` and the
  xattr readers get fuzzed at all: none of them takes a byte slice, so
  none can be a target on its own. `every_committed_image_opens_and_lists_its_root`
  keeps the seeds honest — a seed that stopped opening would go on being
  mutated and go on not failing.

  The gate fails on a hang as well as a panic, naming the target, seed and
  case; on a case count below a floor; and on a `cargo-fuzz` target with
  no counterpart in the gate, so the two tiers cannot drift (#127).

- **ZSTD-compressed images can be read.** `mkfs.erofs -zzstd` produces
  images whose compression algorithm id is 3; before this, opening one
  failed at the superblock's COMPR_CFGS blob with
  `UnsupportedLayout(3)`. The on-disk payload turns out to be an
  ordinary zstd frame, magic number and all, because `mkfs.erofs`
  compresses a pcluster with a plain `ZSTD_compress2` call — unlike the
  other three codecs, which are stored raw. Decoded with `ruzstd`, which
  is pure Rust.
- The `z_erofs_zstd_cfgs` record in the COMPR_CFGS blob is parsed and
  exposed as `ComprCfgs::zstd`. Nothing consults its window log: it
  exists for a decoder that must size a ring buffer before it sees the
  stream, which this crate is not. Both its fields are range-checked
  anyway — an out-of-range value means the blob is not laid out the way
  this reader believes, and every codec after it would then be read
  from the wrong offset.

### Changed

- **Every Linux thing runs in the fs-linux-test-harness VM.** This
  repository referenced the harness nowhere, and exactly one suite here
  ever put an image in front of a real kernel (#128). The oracle tools —
  `mkfs.erofs`, `fsck.erofs`, `dump.erofs` — now run in a Debian guest
  that `scripts/vm-setup.sh` builds erofs-utils 1.9.1 into from source,
  with every codec; the kernel oracles loop-mount there; and the
  fixtures are made there. Nothing filesystem-specific is installed on a
  host, and on a host that is not Linux `chore test` runs the whole
  suite inside the guest instead.

  What that removed is more interesting than what it added. The
  kernel-mountability check was a `sudo -n mount -t erofs -o loop` from
  inside an `#[ignore]`d test: it could only run on a Linux runner with
  passwordless sudo, so a developer never ran it, and for most of its
  life it matched the refusal against a list of permission-denied
  wordings and returned ok (#117). The two xattr oracles set their
  attributes with the host's `setfattr` and returned early when that
  failed (#91). `tests/common/mod.rs` probed each tool with `-V` and
  returned false when it was absent — and erofs-utils 1.5, which is what
  Debian packages, does not answer `-V` at all, so on such a machine
  every oracle test returned early and
  `a_freshly_built_zstd_image_reads_back_exactly` reported ok having
  built no image. All of it is gone: `tests/test_contract.rs` refuses a
  test that spawns an oracle tool itself, mounts a filesystem itself, or
  prints a skip notice.

- **The suite is tiered, budgeted and floored.** `chores.yml` had one
  `cargo test`; CI had three `cargo test` steps and a green run printed
  6,035 lines across its jobs and kept none of them (#131). There are
  now seven tiers, each running through `scripts/tier.sh`, each writing
  its full output to `tmp/logs/<tier>.log` and printing one verdict
  line. A tier that prints more than its measured budget fails with
  status 65; a tier that executes fewer tests than its measured floor
  fails too, because a suite that stopped early reports no failures at
  all and only a count can see that (#129). Every CI job that runs a
  tier uploads its logs with `if: always()`.

  `scripts/output-budget.sh` is deliberately **not** committed here:
  `scripts/tier.sh` asks `cargo metadata` where `am-fs-core` is and
  copies core's copy in for the run (antimatter-studios/rust-fs-core#153).
  The `am-fs-core` pin moves to **v0.2.13**. v0.2.11 is the first release
  that ships the wrapper at all, but v0.2.11 and v0.2.12 read the log
  back aloud when a tier fails — forty lines of tail — and v0.2.13 is
  where a failure became one line naming the log. Both answer the
  `--version` contract, so pinning v0.2.12 would have resolved correctly
  while quietly giving up the behaviour the tiers are for, and only on
  the failure path.

### Removed

- **The `validate mkfs_erofs (fsck.erofs strict)` job**, and the 60-line
  source build of erofs-utils that `ci.yml` and `release.yml` each
  carried. What the job did by hand is the `oracle` tier, on every
  architecture the tier runs on, with its evidence in the tier log.

- **A perf demo that asserted nothing.** `pcluster_cache_perf_demo` timed
  a hundred reads with the cache on and off and printed the result;
  `#[ignore]` with no reason kept it out of a default run while CI's
  `--ignored` pass ran it on every push. The claim it was making is made
  with an assertion by `compressed_file_sequential_reads_use_cache`.

- **`chore test:gsi`'s output budget is provisional, not measured**, and
  antimatter-studios/rust-fs-erofs#133 is why: the `system.img` the pinned
  Android GSI hands over is **ext4**, not EROFS — checked at the
  superblock and confirmed by `dump.erofs` refusing it — so the tier
  cannot be made to pass here, and a passing run is what a budget has to
  be measured from. It had been invisible because that suite was
  `#[ignore]`-gated *and* printed a skip notice; removing the skip is
  what surfaced it. The tier is in neither `chore test` nor CI.

### Security

- A ZSTD frame's declared window size is checked against the format's
  own one-mebibyte dictionary limit **before** a decoder is built. A
  decoder allocates its sliding window from the frame header, and the
  header is bytes off the disk: `ruzstd` would honour a request up to a
  hundred megabytes, and the zstd format can express nearly four
  terabytes. Since the decompression cache misses outside its lock, a
  crafted image could otherwise drive one such allocation per read.

## [0.1.5] — 2026-09-06

### Fixed

- The sizes an image declares are bounded before anything is allocated
  on them: a symlink's length and a pcluster's compressed size are both
  the image's own numbers.
- The lcluster walk stops where a pcluster can no longer reach, rather
  than running to the end of the map.
- A pcluster's decoded span is bounded by its size on disk rather than
  by what it claims to decode to.

## [0.1.4] — 2026-09-04

### Changed

- **The superblock and inode layouts have names**, and each repeated rule has
  one home instead of being restated at every site that depends on it.
- **One xattr entry header.** There were three copies and the third validated
  less than the other two, so which one parsed your image decided whether a
  malformed entry was caught.
- **One in-memory test device, not seven.** Seven had drifted apart on what a
  short read means, so a driver's tests were not all testing the same device.
- One definition of the compacted-pack geometry.

## [0.1.3] — 2026-08-29

### Fixed

- **A pcluster extent that does not contain the requested offset is refused**
  rather than used, which had been returning bytes from the wrong place.
- **The compressed block-fill loop no longer spins forever on a zero-length
  pcluster.** A crafted image could hang the reader.
- The metadata area is laid out with one cursor walk instead of two that could
  disagree.

### Added

- The toolchain is pinned, which this crate had never done — it was the one
  crate in the family free to build with whatever compiler CI happened to have.

## [0.1.2] — 2026-08-25

### Fixed

- The C ABI reports the right `errno` when a path is missing, and is covered by
  tests.

### Changed

- Dependencies are pinned and locked across the CI gates, with an authoritative
  `cargo --locked` stale-lock check.

## [0.1.1] — 2026-06-21

### Added

- **The C ABI surface for FFI consumers.**
- Round-trip, oracle-compatibility, stress and CLI tests, validated against the
  reference EROFS toolchain built from source (the distro package is too old).

### Fixed

- `mkfs` emits a superblock checksum and dirent ordering the reference checker
  accepts.
- Compression cfgs and available algorithms are declared for the non-LZ4
  codecs.

### Changed

- Package renamed to `am-fs-erofs`; the lib name stays `fs_erofs`.

[Unreleased]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/antimatter-studios/rust-fs-erofs/releases/tag/v0.1.1
