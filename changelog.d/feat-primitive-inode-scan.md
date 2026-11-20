### Added

- **`primitives::scan_inodes` finds candidate inodes by scanning the
  metadata area, with or without the superblock (#322).** Every 32-byte
  slot in a NID range that decodes with no findings and has a link is
  handed to a sink as `InodeEvidence`; the sink can stop the scan, and
  the `ScanReport` says where to resume and which stretches of the
  source could not be read. With the superblock destroyed, a
  `Geometry::hypothesis` with the metadata area at block 0 finds every
  inode at its byte offset / 32.
