# Changelog

Notable changes to `am-fs-erofs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

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
