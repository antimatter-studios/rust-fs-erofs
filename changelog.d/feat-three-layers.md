### Added

- **The three driver API layers of rust-fs-core 0.5 (`fs_core::layers`)
  (#311).** `layers::ErofsVolume` is `VolumeAPI` and `OpenVolume` (open
  strictly, or for salvage, which never fails), `PrimitiveAPI` and
  `FileAPI`; `layers::ErofsSalvage` is the `SalvageAdapter`, missing only
  `DeletedCandidates` and `DeletedExtents`, which a read-only format has
  no use for. Candidates are inode slots (detail: the NID); a walk lists
  the file's runs in file order, holes left as gaps, with compressed data
  as encoded pclusters that `read_encoded` decompresses; the tree comes
  from every directory candidate's entries, with each entry's raw name;
  damage is
  every finding of `primitives`, from the superblock down through what
  the root reaches. `ErofsSalvage::from_hypothesis` takes a caller's
  geometry when the superblock is gone.
- `primitives::scan_inodes` keeps a damaged inode as a candidate, and
  requires the format's reserved bits and fields to be zero.

### Changed

- rust-fs-core is pinned at 0.5.1 with its `layers` feature.
