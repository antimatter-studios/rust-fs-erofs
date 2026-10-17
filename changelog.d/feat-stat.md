### Added

- `fs.erofs TARGET stat PATH` reports one path's identity, type, mode,
  owner, link count, size, allocated bytes and effective mtime, with null
  for unrecorded times. Symlinks are reported without following them and
  devices include their major/minor numbers. Inode layout, data layout,
  xattr count and raw flags appear under `erofs` (#210).
