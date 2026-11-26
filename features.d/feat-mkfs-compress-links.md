## Building an image

rename: `mkfs.erofs` tool: an uncompressed image of a directory, any block size
| `mkfs.erofs` tool: an image of a directory, any block size, uncompressed or `-z lz4` / `-z lz4hc[,level]` | Supported; compression since this change | Unreleased (#338) | #306 | `cli.rs`, `cli_mkfs_oracle.rs`, `cli_mkfs_kernel.rs` |
rename: `mkfs.erofs` tool: symlinks, special files and non-UTF-8 names
| `mkfs.erofs` tool: symlinks, FIFOs, sockets, device nodes, and a hardlinked file as one inode | Supported; on Windows special files are left out and hardlinked names are copied | Unreleased (#338) | #306 | `cli.rs`, `cli_mkfs_oracle.rs`, `cli_mkfs_kernel.rs` |
| `mkfs.erofs` tool: names and symlink targets that are not UTF-8 | Partial: left out and named in the report's `skipped` list | 0.3.0 | #306 | `cli.rs` |
