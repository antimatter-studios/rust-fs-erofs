### Added

- **The builder sets the superblock's UUID and build time (#252).**
  `BuildOptions` takes `uuid`, `build_time` and `build_time_nsec`, and
  `mtime_is_build_time` gives every inode the build time as its mtime, as
  `mkfs.erofs -T` does, so the same tree builds to the same bytes.

### Changed

- **An inode whose mtime is the build time is written compact (#252).**
  A compact inode has no mtime of its own and reads as the build time, so
  only a different mtime needs the 64-byte extended inode.

### Fixed

- **A compact inode's mtime reads as the superblock's build time (#252),**
  plus the seconds in its `i_mtime` field, as the kernel reads it. It read
  as zero.
