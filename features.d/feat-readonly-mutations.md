## Interfaces

| `fs.erofs` `rm`, `rmdir`, `mv`, `ln` (including `-s`), `truncate`, `mkdir --parents`, `write --at` | Refused: "EROFS is read-only" (exit 3), before opening the image or reading stdin | Unreleased (#221) | #212 | `tests/cli/test-fs.sh`, `cli_docs.rs` |
