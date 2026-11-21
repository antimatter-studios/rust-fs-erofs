### Added

- **`primitives::inspect_inode` reads the inode at a NID of an image that
  may be damaged (#317).** It never fails: it returns the bytes read, the
  inode decoded field by field when its format word names a data layout,
  and a `Finding` for each thing wrong -- a source too short, an unknown
  data layout, a mode with no file type, an empty directory, a symlink
  target longer than a block, an inline tail that crosses a block, data blocks past the source's end -- with the byte offset
  it is about.
