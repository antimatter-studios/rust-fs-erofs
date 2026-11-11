## Building an image

| Byte-deterministic output | Supported | 0.1.1 | | `lz4hc_writer.rs` (`the_same_tree_compresses_to_the_same_bytes`, `any_thread_count_builds_the_same_bytes`) |
| Library builder: an LZ4HC image within 2% of `mkfs.erofs -zlz4hc,12 -C 32768`, inodes with inline tails packed into the room earlier metadata blocks left | Supported | Unreleased (#259) | | `oracle_size_parity.rs`, `inline_tails.rs` |

## Interfaces

| The tools published to winget | Upcoming: the release job submits each version, but the package is not registered with winget yet | | #281 | |
| `fs.erofs` recursive export policy regression coverage | Supported: exact depth bounds, directory merging, replacement refusal, image offsets and unchanged single-file commands, against the kernel | 0.5.0 (#242) | | `cli_export_tree_kernel.rs` |
