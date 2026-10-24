### Changed

- **`fs.erofs` reports a missing path with the `not_found` error kind
  (#251).** `ls`, `ls -R`, `stat`, `read`, `export` and
  `export --recursive` given a path that is not there, or one whose parent
  is not there, still exit 1 and now carry `"kind": "not_found"` beside
  `code` and `error` in the JSON error. A script can match that across
  drivers, whatever the message says. A path through a regular file and a
  damaged directory keep their plain failure.
- **Built on rust-fs-core 0.4.2**, for `CliError::not_found`.
