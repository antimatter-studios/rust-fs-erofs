### Added

- **The builder writes hard links (#253).** `Node::HardLink { target }`
  is another name for the node at `target`, a path from the root: the
  directory entry names that node's inode, and its `nlink` counts every
  name. A link to nothing, to a directory or to another link is refused
  with `BadInode`. An inode with more names than a compact inode's 16-bit
  count holds is written extended.

### Changed

- **`mkfs::Node` has a new variant, `HardLink` (#253).** A `match` over it
  needs an arm for the new variant.
