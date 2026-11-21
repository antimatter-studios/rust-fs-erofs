### Added

- **`primitives::inspect_dir_block` and `inspect_directory` read a
  directory of an image that may be damaged, block by block (#318).** Every
  entry whose header and name are sound is kept when others in its block
  are not, and each fault is a `Finding` at its byte offset: a first name
  offset that places no entry array, a name outside its block or
  overlapping the one before, an empty name or one holding a NUL or `/`,
  a NID past the source's end, and names out of byte order. A block that
  cannot be read is a finding and the next block is read.
