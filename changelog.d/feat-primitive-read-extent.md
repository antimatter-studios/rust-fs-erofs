### Added

- **`primitives::read_extent` reads the file bytes one extent of a walk
  holds, without trusting anything beyond it (#320).** Bytes stored as is
  are copied, a hole is zeros, a pcluster is read and decompressed whole
  (with the decoders `Filesystem` uses; interlaced and deduplicated pclusters
  included) and the extent's part returned, and a fragment is read through
  the packed inode's own walk. What cannot be read or does not decode is
  an error naming the source range: those bytes are missing, never zeros.
- `ExtentSource::Pcluster` says whether a pcluster is shared as a prefix
  (`partial`) and whether a stored one is interlaced.
