### Added

- **`primitives::inspect_superblock` reads the superblock of an image that
  may be damaged (#316),** the first of the damage-tolerant primitives
  docs/three-layer-api.md plans. It never fails: it returns the bytes read,
  every field as stored, whether the checksum matched, and a `Finding` for
  each thing wrong -- a short source, the magic, the block size, the
  checksum, unknown incompat bits, an image longer than its source, a
  metadata area or root inode past the source's end -- with the byte
  offset it is about. `Geometry` is where the image's structures are,
  stated by the superblock or supplied as a hypothesis when it is
  destroyed, since EROFS keeps no backup.
- **`Superblock::decode`** returns every field of a 128-byte superblock as
  stored, checked for nothing; `Superblock::parse` is it plus the checks.
