### Added
- `fs.erofs TARGET export --recursive PATH DEST` copies regular directory trees through core's shared tree transfer, with per-item reports, a depth bound, existing-directory merging and existing-file refusal unless `--replace` is given. Symlinks and special files are skipped without following them.
- `fs.erofs export --verify` reads staged files back before publication.
