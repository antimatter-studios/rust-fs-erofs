## Reading

| Damaged images: the files the kernel reads are read with its bytes, the ones it cannot are refused | Supported for a destroyed superblock, directory block, inode block or compressed pcluster, and a truncated image | Unreleased (#315) | #311 | `damaged_images_kernel.rs` |
| Hash-sorted directories; one name resolved without decoding the directory | Supported; found by bisecting on each block's first name, as the kernel does, so a damaged block loses only its own names | 0.1.1; lookup cost 0.2.0; bisection Unreleased (#315) | | `lookup_allocations.rs`, `damaged_images_kernel.rs` |
