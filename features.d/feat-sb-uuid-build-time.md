## Reading

| Compact (32-byte) and extended (64-byte) inodes | Supported; a compact inode's mtime is the build time | 0.1.1; mtime Unreleased (#252) | | `oracle_compat.rs`, `fixture_matrix.rs`, `mkfs::tests::mtime_is_build_time_makes_every_inode_compact_with_that_mtime` |

## Building an image

| Library builder: superblock UUID and build time; every mtime the build time (`-T`), with compact inodes | Supported | Unreleased (#252) | | `mkfs::tests::the_superblock_carries_the_requested_uuid_and_build_time`, `oracle_writer.rs` |
