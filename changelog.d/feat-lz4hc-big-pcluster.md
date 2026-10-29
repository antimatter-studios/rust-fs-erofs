### Fixed

- **A compression map header is read and written at the 8-byte boundary
  after the inode's xattrs (#256),** where the kernel and erofs-utils
  place it. When the xattrs ended 4 bytes past a boundary -- a compressed
  file with a small inline xattr, such as an SELinux label -- the reader
  read half a header from the padding and the builder wrote one there,
  so such a file failed to decompress.
