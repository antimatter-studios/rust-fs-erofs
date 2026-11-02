### Changed
- `fs.erofs ls --recursive` uses rust-fs-core's shared recursive-listing arguments, help, defaults and bound-reached errors. The defaults remain depth 64 and 100000 entries, and the EROFS walk and partial reports are unchanged.
