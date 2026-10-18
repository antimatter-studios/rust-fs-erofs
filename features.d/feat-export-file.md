## Interfaces

| `fs.erofs` `export PATH DEST` | Supported: bounded single-file transfer, SHA-256, stdout, replacement refusal and atomic publication; directories, symlinks and special files refused | Unreleased (#226) | #224 | `cli_export_kernel.rs`, `cli_docs.rs` |
| `fs.erofs` recursive export | Supported: shared TreeSource traversal, per-item reports, depth bound, directory merging and existing-file refusal; links and special files skipped | Unreleased (#237) | #232 | `cli_export_tree_kernel.rs` |
