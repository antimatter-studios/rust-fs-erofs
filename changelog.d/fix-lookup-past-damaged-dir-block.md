### Fixed

- **A name in an intact block of a directory is found when another of the
  directory's blocks is damaged, as Linux finds it (#315).**
  `Filesystem::lookup` now bisects on each block's first name, the way the
  kernel's `erofs_find_target_block` does, and reads only the blocks the
  search touches. It used to read and validate every block, so one
  destroyed block lost every name in the directory, where the kernel loses
  only the names in that block. Every block the search reads is still
  validated whole.
