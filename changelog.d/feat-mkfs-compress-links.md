### Added

- **`mkfs.erofs` compresses.** `-z lz4`, `-z lz4hc` and `-z lz4hc,LEVEL`,
  spelled as erofs-utils spells them, compress regular files.
- **`mkfs.erofs` writes links and special files.** Symlinks, FIFOs, sockets
  and device nodes are written as what they are, and the names of a
  hardlinked file as one inode with that many links, where before they were
  left out or copied. The report counts them (`symlinks`, `special_files`,
  `hard_links`) and names the compressor (`compression`).
