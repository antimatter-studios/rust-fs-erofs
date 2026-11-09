## Building an image

| Library builder: LZ4HC compression into big pclusters (`BuildOptions::compression`, as `-zlz4hc,12 -C 32768`), stored blocks and stored files where compression saves nothing | Supported | Unreleased (#256) | | `lz4hc_writer.rs`, `streaming_memory.rs` |
| Library builder: big pclusters in the compacted index | Not supported: big pclusters use the legacy index | | | |
