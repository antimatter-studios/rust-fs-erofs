### Fixed

- **A compression map header is read and written at the 8-byte boundary
  after the inode's xattrs (#256),** where the kernel and erofs-utils
  place it. When the xattrs ended 4 bytes past a boundary -- a compressed
  file with a small inline xattr, such as an SELinux label -- the reader
  read half a header from the padding and the builder wrote one there,
  so such a file failed to decompress.

### Added

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
- **Built with liblz4, through `lz4-sys` (#256).** It is the LZ4HC
  encoder `mkfs.erofs` uses, and the only one with the fixed-output mode
  a pcluster needs. Building the crate needs a C compiler.

### Changed

- **A legacy compression index records `delta[1]` (#256),** the lclusters
  from each NONHEAD entry to the next head, where it wrote zero.
