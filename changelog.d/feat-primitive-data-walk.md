### Added

- **`primitives::walk_data` maps a file's bytes to where they are stored,
  as far as its map can be followed (#319).** It reads every layout --
  `FLAT_PLAIN`, `FLAT_INLINE`, `CHUNK_BASED` and both compressed ones --
  and returns `DataExtent`s: bytes stored as is on a device, an inline
  run in the metadata area, a declared hole, a pcluster with its codec
  and the file bytes it decodes to, or a fragment of the packed inode. A
  map that cannot be followed, or that points past the source's end, ends
  the walk as `WalkEnd::Broken` with a finding, keeping the extents
  before it; nothing after it is invented.
