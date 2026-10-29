### Added

- **The builder streams (#255).** `mkfs::build_image_to` writes an image
  into any `Write + Seek` sink, and `Node::StreamedFile` takes a file's
  bytes from a `Content`: its size, known up front, and a reader opened
  once when the writer reaches it. Each file's bytes are copied through
  one 128 KiB buffer into the image as they are read, so a build holds
  the tree's metadata in memory and never its file data: 192 MiB of
  file content builds with a peak heap under 24 MiB. Content that ends
  before its size, or goes on past it, fails the build.

### Changed

- **File data comes before the metadata area (#255).** The data area now
  starts at the block after the superblock, and the metadata area and
  the directories follow it; the superblock's `meta_blkaddr` says where.
  Every reader goes through that field, so images read as they did, but
  the builder's bytes differ from earlier releases for the same tree.
  `build_image` and `build_image_with` are `build_image_to` writing into
  a `Vec`, and the same tree still builds to the same bytes.
- **`mkfs::Node` has a new variant, `StreamedFile` (#255).**
