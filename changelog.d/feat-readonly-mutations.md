### Added
- `fs.erofs` recognises `rm`, `rmdir`, `mv`, `ln` (including `-s`), `truncate`, `mkdir --parents` (`-p`) and `write --at`. Every mutation refuses with exit 3 and the existing "EROFS is read-only" message before opening the image or reading stdin; help, man pages and completions include the new forms.
