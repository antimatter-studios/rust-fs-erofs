# Changelog

Notable changes to `rust-fs-erofs` (published as `am-fs-erofs` until its last version), newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [0.4.0] — 2026-10-06

### Changed

- **Published as `rust-fs-erofs`, the repository's name.** The crate was `am-fs-erofs`
  until its last version, which stays on crates.io pointing here. A
  dependent changes one line in `Cargo.toml`; the import (`fs_erofs`) and the C symbols are unchanged.
- **Depends on `rust-fs-core` 0.3.0**, the same library under its new name.

## [Unreleased]

### Changed

- **The family's scripts run in place from rust-fs-core, and this repository
  keeps no copy.** `scripts/core.sh` and `scripts/tier.sh` are gone; CI and
  chores run `../rust-fs-core/scripts/NAME.sh` at the pinned version
  (rust-fs-core 0.3.2, #212).
- **A release's notes are its CHANGELOG section**, and a tag the CHANGELOG
  does not describe stops before anything is published (rust-fs-core#209).

## [0.3.1] — 2026-10-06

### Renamed

- **The last version published as `am-fs-erofs`.** The crate is renamed to
  `rust-fs-erofs`, the repository's name; every later version is published under
  that name only, starting at 0.4.0. The description and the README say where
  the crate went. The import is unchanged: `use fs_erofs::...` keeps working.


### Changed

- **The release tarballs are packaged, attested and attached by
  rust-fs-core's `release-cli` workflow, not by a copy here** (#181).
  `release.yml`'s `package-cli` and `release-cli` jobs become one `cli` job
  calling `antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml`
  at v0.2.23, pinned by commit SHA; `scripts/package-cli.sh` and its test
  are gone, and `ci.yml` packages through `scripts/core.sh package-cli`.
  What ships is declared in `Cargo.toml`'s `[package.metadata.package-cli]`
  and the tarball's layout is unchanged. The attestations now name the
  shared workflow, so a tarball is verified with `--signer-workflow
  antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml`.
  am-fs-core moves to v0.2.23, the first release that carries it.

- **A test run gives the harness VM and its machine-wide slot back when it
  ends, however it was started.** The oracle and kernel tests boot the VM
  from their own process, and stopping it was left to chore's `after_all`
  reaper, which runs only inside a chore invocation of this repository: a
  run made any other way (`scripts/test.sh` by hand, `scripts/tier.sh`) exited
  with the VM idle and the slot held, and every other repository's VM work
  queued behind it until the guest's idle deadline. `scripts/test.sh` now runs
  cargo through the harness's `vm.sh session`, which brings the VM down
  and releases the slot when the run ends — passed, failed or killed —
  leaves a VM held with `chore vm:up` alone, and runs cargo as it is in the
  guest and on a host that cannot run the VM. The harness moves to v0.3.0,
  the release that provides it (fs-linux-test-harness#37, #36).

- **The real-world Android suite reads an EROFS image, and CI runs it
  nightly** (#54). It was `chore test:gsi` over an Android GSI whose
  `system.img` is ext4 — true of every GSI Google publishes, Android 16
  and 17 — so it could never pass (#133). It is now `chore test:android`
  over the `system_dlkm` partition of Google's Android 16 emulator image,
  fetched by `tests/fixtures/download-android-erofs.sh`, which refuses a
  cut that is not EROFS and names what it found. `tests/oracle_android.rs`
  compares all 108 inodes — type, mode, owner, size, content hash and
  xattrs — with what the Linux kernel's EROFS driver and `fsck.erofs`
  read, instead of probing for a few well-known paths and skipping what
  it could not read. `.github/workflows/android.yml` runs the tier
  nightly with the fixture cached on its SHA-256. Its output budget is
  the runner's cold build, 4,005 bytes measured there; the first nightly
  run exceeded a budget measured on a host with no crates to download and
  no colour (#166).

## [0.3.0] — 2026-09-30

### Breaking

- **In-image paths cross the C ABI as bytes, not UTF-8 (#148).**
  `fs_erofs_stat`, `_dir_open`, `_read_file` and `_readlink` read their
  `const char *` as the bytes up to the NUL and compare them byte for byte
  against the names in the image; they no longer decode it. A path that
  did not decode used to become `""`, which resolved to the root, so
  `fs_erofs_stat` reported the root directory's attributes and
  `fs_erofs_dir_open` listed the root for a name that was merely not
  UTF-8. Such a name is now looked up, and a path naming nothing is
  `ENOENT`, never the root.

  **Source-compatible for every caller passing UTF-8**, because UTF-8 is a
  byte string too. `Filesystem::lookup_path_bytes` is the resolution and
  `lookup_path(&str)` wraps it, so the Rust API is unchanged.
  `fs_erofs_mount` keeps its UTF-8 decode: its argument is a path on the
  host filesystem, not an in-image name.

### Added

- **The command-line tools are one multi-call binary, `rust-fs-erofs`**,
  behind a new `cli` feature, so the static library gains no dependency
  from them. It dispatches on the name it is started under: `mkfs.erofs`
  and `rust-fs-erofs mkfs` are the same program. `--version` prints
  `<tool> (am-fs-erofs) <version>`, a result is JSON on stdout (`--text`
  for people) and a failure is `{"error": ..., "code": N}` on stderr.
- **`fs.erofs <image> ls|read|get|info`** reads an image directly, every
  layout and compressor the library reads: JSON entries (a symlink's with
  its `target`), raw bytes for `read` (`-o FILE` too), and the shared
  envelope (`fs`, `label`, `total_bytes`, `free_bytes`, `block_size`,
  `dirty`, with `erofs.*` nested). `--offset` reaches a filesystem inside a
  larger image. `write`, `mkdir`, `set` and `resize` answer "EROFS is
  read-only" with status 3.
- `rust-fs-erofs doctor` says, for every name, whether the program `PATH`
  finds is this one, and when it is not (erofs-utils' `mkfs.erofs`, say)
  what wins and the fix.
- A `cli` tier (`chore cli:install`, `chore test:cli`) tests the tools as
  installed, doctor first, and a `cli` CI job runs it on every pull request.
- **Release tarballs of the tools**, `am-fs-erofs-<version>-<platform>.tar.gz`
  for `darwin-arm64` and `linux-x86_64`, attached to the GitHub release for
  the tag with a build-provenance attestation. Each is an install prefix:
  `bin/rust-fs-erofs` with `mkfs.erofs` and `fs.erofs` as relative symlinks
  to it, man pages (section 8 for `mkfs.erofs`, 1 for the rest), zsh, bash
  and fish completions, `share/rust-fs-erofs/CAVEATS` and `LICENSE`. The
  binary writes its own man pages and completions (`rust-fs-erofs generate`),
  so they cannot describe a flag it does not take, and the `cli` CI job
  packages and checks the tarball on every pull request.

- Releases carry a build-provenance attestation: the published `.crate` is
  attached to the GitHub release for its tag, checked first against the
  crates.io checksum, and verifiable with `gh attestation verify` (see the
  README, "Verifying a release").

### Fixed

- **Deduplicated images read in full (#125).** `mkfs.erofs -Ededupe`
  reuses the start of a pcluster another extent already wrote, and marks
  the extent's head `Z_EROFS_LI_PARTIAL_REF` when it needs fewer bytes
  than the pcluster decodes to. Every pcluster was decoded to exactly its
  extent's length, which a reused one does not have, so LZ4 and LZMA
  refused between 2 and 15 of 40 files in `-Efragments,dedupe`,
  `-Efragments,ztailpacking,dedupe` and `-Eall-fragments,dedupe` images.
  Such an extent is now decoded as a prefix, as the kernel does, and only
  such an extent: anywhere else a frame longer than its extent is still
  refused. Checked against the files' own bytes and, for LZ4, against
  the Linux driver's reading of the same images.

### Changed

- **`mkfs_erofs` is now `mkfs.erofs`**, the `mkfs` arm of the multi-call
  binary, with the same argument order (`OUTPUT SOURCE`) and `-b` as a
  short form of `--block-size`. It prints a JSON report read back from the
  image it wrote, naming each source entry it left out and why. `--label`
  is refused (exit 3) until the builder takes a volume name. The
  `mkfs_erofs` target is gone; it was never in a release archive, so no
  alias is kept.

- **The file in an `-Eall-fragments,ztailpacking` inline tail is refused
  rather than read (#125).** erofs-utils 1.9.1 writes that tail so that
  no reader following the format gets the file back: the Linux 6.1
  driver, `fsck.erofs --extract` and this crate all read it differently
  from its source, this crate by returning zeros for its missing end.
  Reading those bytes now fails with an error naming the shape. Bytes of
  the same file before the tail, and every other file in the image,
  still read. The README's support table says so, as does the crate
  description.

## [0.2.0] — 2026-09-27

### Breaking

- **`fs_erofs_readlink` returns the target's length, not 0 (#138).** The
  readlink contract is now the one every driver in the family shares:
  success returns the length in bytes, NUL not counted, as Linux
  `readlink(2)` does, and writes the target plus a NUL. A buffer smaller
  than length + 1 is still refused with `ERANGE` and nothing written, and
  the error message now names the size needed; a zero-byte buffer is
  `ERANGE` rather than `EINVAL`. NULL `fs`/`path`/`buf` is `EINVAL`.
  Callers testing `== 0` for success must test `>= 0`. The version moves
  to 0.2.0.

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

[0.4.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/antimatter-studios/rust-fs-erofs/releases/tag/v0.1.1
