### Changed

- **The builder puts the tail of a directory, a symlink and an
  uncompressed file inline, after its inode (#257),** whenever it fits in
  the metadata block the inode sits in (`FLAT_INLINE`), as `mkfs.erofs`
  does. A small directory or a symlink took a whole data block, and a
  file larger than a block one more for its last few bytes; 200
  directories each holding a symlink and a 5000-byte file now take under
  300 blocks where they took over 800. A file the compressor stores
  uncompressed has its tail inline too, without being read twice.
