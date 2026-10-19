### Added
- `fs.erofs export --recursive --preserve=links,times` recreates hard links using stable inode identity, copies opaque symlink targets without following them, and preserves available modification times. Extended inodes provide their stored time and compact inodes use the image build time. Reports identify unavailable access times, unpreserved symlink times and targets that escape the destination.

### Changed
- Recursive export without `--preserve=links` refuses each symlink with a preservation hint; special files remain skipped.
