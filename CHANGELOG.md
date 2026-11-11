# Changelog

Notable changes to `rust-fs-erofs` (published as `am-fs-erofs` until its last version), newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

Changes not yet released are fragments in [`changelog.d/`](changelog.d/), one per
pull request; a release gathers them into a section here.

## [Unreleased]

## [0.6.0] — 2026-11-11

### Added

- **`XattrSpec::from_full_name` builds an xattr from a full name such as
  `security.selinux` or `user.foo` (#263),** taking the namespace index
  from the same prefix table the reader resolves names with, and
  `xattr::split_full_name` is that inverse of `resolve_full_name`. The
  POSIX ACL names map to their indices with an empty name; a name with no
  known prefix is refused with `BadXattr`, not kept whole under the raw
  namespace.

- **The builder writes hard links (#253).** `Node::HardLink { target }`
  is another name for the node at `target`, a path from the root: the
  directory entry names that node's inode, and its `nlink` counts every
  name. A link to nothing, to a directory or to another link is refused
  with `BadInode`. An inode with more names than a compact inode's 16-bit
  count holds is written extended.

- **The builder compresses regular files as `mkfs.erofs -zlz4hc,12 -C
  32768` does (#256).** `BuildOptions::compression` takes a `Compression`
  -- an LZ4HC level and the most compressed bytes one pcluster holds,
  `Compression::LZ4HC_12_32K` for that command -- and every `Node::File`
  and `Node::StreamedFile` is compressed as it is read. Each pcluster
  takes as much of the file as LZ4HC fits in it, so pclusters are big
  (`BIG_PCLUSTER`) and need not start on a block; a block that would
  save nothing compressed is stored as it is, and a file where that is
  true throughout is stored uncompressed, without being read twice. The
  index is the legacy (full) format.
- **LZ4HC comes from `am-lz4-flex` (#256, #269),** a fork of `lz4_flex`
  published until upstream releases PSeitz/lz4_flex#244, which adds the HC
  and fixed-output modes a pcluster needs, so the crate stays pure Rust.

- **The builder compresses on every core (#262).** `BuildOptions::threads`
  sets how many threads compress, 0 for one per core, the default. Each
  file is compressed in 4 MiB segments that start fresh pclusters, read
  once in order and written in order, so any thread count builds the same
  bytes; a segment boundary costs under 0.1% of image size. Each thread adds
  about 8 MiB of memory; with one thread nothing is spawned.

- **The builder sets the superblock's UUID and build time (#252).**
  `BuildOptions` takes `uuid`, `build_time` and `build_time_nsec`, and
  `mtime_is_build_time` gives every inode the build time as its mtime, as
  `mkfs.erofs -T` does, so the same tree builds to the same bytes.

- **The builder shares repeated xattrs (#254).** With
  `BuildOptions::share_xattrs`, an xattr two or more inodes carry -- the
  same namespace, name and value -- is written once in the shared xattr
  area, and each of those inodes holds a 4-byte reference to it, as
  `mkfs.erofs` does. Off by default, so existing callers build the same
  bytes.

- **The builder streams (#255).** `mkfs::build_image_to` writes an image
  into any `Write + Seek` sink, and `Node::StreamedFile` takes a file's
  bytes from a `Content`: its size, known up front, and a reader opened
  once when the writer reaches it. Each file's bytes are copied through
  one 128 KiB buffer into the image as they are read, so a build holds
  the tree's metadata in memory and never its file data: 192 MiB of
  file content builds with a peak heap under 24 MiB. Content that ends
  before its size, or goes on past it, fails the build.

### Changed

- **`Content` and `Node` take a lifetime, `Content<'a>` and `Node<'a>`
  (#263),** for which a file's opener and reader may borrow -- files read
  out of another open image, say. Both must be `Send`, so a `Node` stays
  `Send`. `build_image`, `build_image_with` and
  `build_image_to` take a `Node` of any lifetime. A function that names
  `Node` in its return type now names its lifetime, `Node<'static>` for
  a tree that owns everything; nothing else changes for a caller.

- **`mkfs::Node` has a new variant, `HardLink` (#253).** A `match` over it
  needs an arm for the new variant.

- **The builder puts the tail of a directory, a symlink and an
  uncompressed file inline, after its inode (#257),** whenever it fits in
  the metadata block the inode sits in (`FLAT_INLINE`), as `mkfs.erofs`
  does. A small directory or a symlink took a whole data block, and a
  file larger than a block one more for its last few bytes; 200
  directories each holding a symlink and a 5000-byte file now take under
  300 blocks where they took over 800. A file the compressor stores
  uncompressed has its tail inline too, without being read twice.

- `fs.erofs ls --recursive` uses rust-fs-core's shared recursive-listing arguments, help, defaults and bound-reached errors. The defaults remain depth 64 and 100000 entries, and the EROFS walk and partial reports are unchanged.

- **A legacy compression index records `delta[1]` (#256),** the lclusters
  from each NONHEAD entry to the next head, where it wrote zero.

- **An inode with an inline tail fills the room an earlier metadata block
  was left with (#259).** An inode and its tail must share one block, and
  the builder placed each one after the last, starting a new block when
  it did not fit, so a tree of small files wasted up to a block apiece.
  It now puts each one in the block with the least room that still holds
  it, as `mkfs.erofs` does: on about 2600 small files its image went from
  6% larger than `mkfs.erofs -zlz4hc,12 -C 32768`'s to within 1%.

- **An inode whose mtime is the build time is written compact (#252).**
  A compact inode has no mtime of its own and reads as the build time, so
  only a different mtime needs the 64-byte extended inode.

- **File data comes before the metadata area (#255).** The data area now
  starts at the block after the superblock, and the metadata area and
  the directories follow it; the superblock's `meta_blkaddr` says where.
  Every reader goes through that field, so images read as they did, but
  the builder's bytes differ from earlier releases for the same tree.
  `build_image` and `build_image_with` are `build_image_to` writing into
  a `Vec`, and the same tree still builds to the same bytes.
- **`mkfs::Node` has a new variant, `StreamedFile` (#255).**

- The builder uses the faster LZ4HC encode path and a level-12 search budget
  scaled to cluster capacity, with benchmark checks guarding compressed output
  size against liblz4.

### Fixed

- **A compression map header is read and written at the 8-byte boundary
  after the inode's xattrs (#256),** where the kernel and erofs-utils
  place it. When the xattrs ended 4 bytes past a boundary -- a compressed
  file with a small inline xattr, such as an SELinux label -- the reader
  read half a header from the padding and the builder wrote one there,
  so such a file failed to decompress.

- **A compact inode's mtime reads as the superblock's build time (#252),**
  plus the seconds in its `i_mtime` field, as the kernel reads it. It read
  as zero.

- **The crate can be published again (#269).** Its LZ4HC encoder came from a
  git dependency on a fork of lz4_flex, which crates.io refuses, so no
  release could be made after #252. The encoder now comes from that fork
  published as `am-lz4-flex` 0.14.0, pinned exactly, until upstream releases
  PSeitz/lz4_flex#244. Its fixed-output blocks are byte-identical to the
  fork commit used before, so the images built do not change.

- **A directory block holds "." and ".." in byte order among its other
  names (#258).** The builder put them first, so a directory holding a
  name that sorts before "." -- `+x`, `-x`, `,x`, as Android trees have
  -- was refused by `fsck.erofs` ("wrong dirent name order") and could not
  be searched by the kernel.

- **`chore staticlib` copies the archive from where cargo built it.** The task
  read a fixed `target/` while cargo honoured `CARGO_TARGET_DIR` and
  `build.target-dir`, so the copy failed after a good build, or shipped an old
  archive left in `target/` without an error (#282).

## [0.5.1] — 2026-11-02

### Changed

- **`fs.erofs` reports a missing path with the `not_found` error kind
  (#251).** `ls`, `ls -R`, `stat`, `read`, `export` and
  `export --recursive` given a path that is not there, or one whose parent
  is not there, still exit 1 and now carry `"kind": "not_found"` beside
  `code` and `error` in the JSON error. A script can match that across
  drivers, whatever the message says. A path through a regular file and a
  damaged directory keep their plain failure.
- **Built on rust-fs-core 0.4.2**, for `CliError::not_found`.

- **A partial tree export exits 5.** With rust-fs-core 0.4.3, a recursive export
  that published what it could and reported the rest as refused or failed exits
  with the partial status instead of 1.

### Fixed

- An image of 1 KiB blocks or less no longer carries a superblock checksum, as `mkfs.erofs` writes none there. `fsck.erofs` 1.8.6 crashed on a 512-byte image that had one. The reader still verifies images from older tools that do.

- **An inlined compressed tail sets the ztailpacking feature (#266).** Without
  that bit in the superblock, Linux 6.1 could not read such a file and returned
  an I/O error.

- **The Windows tools start without the Visual C++ runtime.** rust-fs-core 0.4.3
  links the C runtime statically into the Windows release zips, which did not
  start on a Windows install without it.

## [0.5.0] — 2026-10-19

### Added

- `fs.erofs TARGET export PATH DEST [--replace]` exports regular files through the shared bounded transfer, reporting SHA-256 and publishing local files only when complete. (#226)

- `fs.erofs export --recursive --preserve=links,times` recreates hard links using stable inode identity, copies opaque symlink targets without following them, and preserves available modification times. Extended inodes provide their stored time and compact inodes use the image build time. Reports identify unavailable access times, unpreserved symlink times and targets that escape the destination. (#241)

- `fs.erofs TARGET import SOURCE PATH [--replace]` refuses with exit 3 and the existing EROFS read-only message before opening the image or source file or consuming stdin. (#228)

- `fs.erofs ls --recursive` (`-R`) walks directories in byte-sorted depth-first order, with finite depth and entry bounds, symlink safety and repeated-directory detection. Bound and read failures retain a partial report and exit 1. (#219)

- `fs.erofs` recognises `rm`, `rmdir`, `mv`, `ln` (including `-s`), `truncate`, `mkdir --parents` (`-p`) and `write --at`. Every mutation refuses with exit 3 and the existing "EROFS is read-only" message before opening the image or reading stdin; help, man pages and completions include the new forms. (#221)

- `fs.erofs TARGET export --recursive PATH DEST` copies regular directory trees through core's shared tree transfer, with per-item reports, a depth bound, existing-directory merging and existing-file refusal unless `--replace` is given. Symlinks and special files are skipped without following them. (#237)
- `fs.erofs export --verify` reads staged files back before publication. (#237)

- `fs.erofs TARGET stat PATH` reports one path's identity, type, mode,
  owner, link count, size, allocated bytes and effective mtime, with null
  for unrecorded times. Symlinks are reported without following them and
  devices include their major/minor numbers. Inode layout, data layout,
  xattr count and raw flags appear under `erofs`. (#217)

- `fs.erofs IMAGE xattr PATH [NAME]` lists extended attributes as JSON or text and reads raw values to stdout or `-o FILE`, preserving non-UTF-8 names and values across inline, shared and prefix-dictionary storage. (#222)

### Changed

- **rust-fs-core 0.3.15.** The crate builds against the core release that
  carries the shared tree transfer (`TreeSource`, `TreeMetadata` and
  `export_tree`), which recursive export will adapt; the release workflow,
  branch reaping and the fuzz clone call that release. (#234)

- A pull request runs a quick CI profile (unit, lint, the CI profile truth table and semver); the merge queue, main, tags and a manual run keep every full leg, including the fixture VM, the oracle and kernel tiers, arm64, the suite in the guest and the Linux and Windows CLI tiers. (#205)

- Every rust-fs-core pin now names 0.3.14, which supplies the shared single-file transfer. (#226)

- Recursive export without `--preserve=links` refuses each symlink with a preservation hint; special files remain skipped. (#241)

## [0.4.1] — 2026-10-11

### Added

- **`docs/features.md`, a features page kept current by every pull request.**
  Each feature's state, the release it shipped in, its tracking issue and the
  test that checks it. The README's capability tables are a short summary
  pointing to it.
- **The tools are built and released for Windows (#185).** A release now
  attaches `rust-fs-erofs-<version>-windows-x86_64.zip` and `-windows-arm64.zip`
  beside the tarballs, packaged by rust-fs-core 0.3.9's release-cli: the
  binary as `bin/rust-fs-erofs.exe` and each of `fs.erofs` and `mkfs.erofs` a
  copy of it, since a symlink needs Developer Mode or an administrator there.
  A `cli-windows` CI job builds that zip on `windows-latest`, unpacks it with
  `Expand-Archive` and runs the same CLI suite against those tools first on
  PATH. Publishing to winget is still to come.

### Fixed

- **A cold cargo cache no longer fails the android tier's output budget.**
  `chore test:android` compiles its test before the budgeted run starts
  counting, as the unit tier already does, so the nightly run after a
  `Cargo.lock` change no longer fails with its test passing (#195).
- **A cold cargo cache no longer fails the unit tier's output budget.**
  `chore test:unit` compiles its tests before the budgeted run starts
  counting, so the toolchain install, downloads and `Compiling` lines are
  no longer counted against the tier. The budget goes back to 740 lines /
  40,000 bytes, sized for the test output alone (#189).
- **A transient HTTP 5xx from the chore release download no longer fails a CI
  job.** `scripts/ci-install-chore.sh` retries both downloads up to five times
  on any error; the checksum check still guards what was fetched.

### Changed

- **The family's scripts run in place from rust-fs-core, and this repository
  keeps no copy.** `scripts/core.sh` and `scripts/tier.sh` are gone; CI and
  chores run `../rust-fs-core/scripts/NAME.sh` at the pinned version
  (rust-fs-core 0.3.2, #212).
- **A release's notes are its CHANGELOG section**, and a tag the CHANGELOG
  does not describe stops before anything is published (rust-fs-core#209).

## [0.4.0] — 2026-10-06

### Changed

- **Published as `rust-fs-erofs`, the repository's name.** The crate was `am-fs-erofs`
  until its last version, which stays on crates.io pointing here. A
  dependent changes one line in `Cargo.toml`; the import (`fs_erofs`) and the C symbols are unchanged.
- **Depends on `rust-fs-core` 0.3.0**, the same library under its new name.

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

[0.6.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/antimatter-studios/rust-fs-erofs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/antimatter-studios/rust-fs-erofs/releases/tag/v0.1.1
