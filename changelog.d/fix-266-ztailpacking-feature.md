### Fixed

- **An inlined compressed tail sets the ztailpacking feature (#266).** Without
  that bit in the superblock, Linux 6.1 could not read such a file and returned
  an I/O error.
