### Added

- `fs.erofs ls --recursive` (`-R`) walks directories in byte-sorted depth-first order, with finite depth and entry bounds, symlink safety and repeated-directory detection. Bound and read failures retain a partial report and exit 1.
