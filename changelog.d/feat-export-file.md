### Added

- `fs.erofs TARGET export PATH DEST [--replace]` exports regular files through the shared bounded transfer, reporting SHA-256 and publishing local files only when complete.

### Changed

- Every rust-fs-core pin now names 0.3.14, which supplies the shared single-file transfer.
