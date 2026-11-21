### Added

- **`primitives::inspect_xattrs` reads an inode's extended attributes in
  an image that may be damaged, keeping every one that decodes (#321).**
  Inline entries are read in order up to the first that does not fit,
  and each shared entry on its own, so one bad shared entry or shared id
  costs only itself; each fault is a `Finding` at its byte offset.
