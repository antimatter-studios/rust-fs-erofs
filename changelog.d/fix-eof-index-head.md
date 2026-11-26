### Fixed

- **A compressed file that ends inside a block ends its index with a head
  there.** `BuildOptions::compression` left the last lcluster of such a
  file a NONHEAD of its last pcluster. A Linux 5.10 kernel then
  decompresses that pcluster to the end of the block the file ends in,
  which is more than its data holds, and the read fails with EIO. The
  index now ends with a PLAIN head at the end of the file, as `mkfs.erofs`
  writes it.
