### Added

- **A file's compressed data can be taken from another image as it is.**
  `Transplant::of` describes the LZ4 pclusters an open image stores a file
  in, and `mkfs::Node::TransplantedFile` writes them to the image being
  built, under an index of its own, so an image made of another's files
  neither decompresses nor compresses the ones it did not change. A file
  stored uncompressed is taken as its blocks, stored as they are, and is
  uncompressed in the image built of it too, so a file its source found did
  not compress is not compressed again. A file stored as the builder does
  not write one -- another codec, ztailpacking,
  fragments, a deduplicated extent, interlaced blocks -- is `None`, to be
  read and written as any other.
