# What a read costs

Measured by `tests/read_path_cost.rs`, which counts **calls to the
device** rather than wall time. Wall time on a laptop with a warm page
cache says more about the laptop than the driver; call counts are
deterministic — the same image walked the same way makes the same calls
every time — so they can be compared across months and asserted on.

Wall time is printed beside them because it is what a user feels. It is
not what anything is judged by.

The fixture is built by the test itself, in memory, using this crate's
own `mkfs`: a tree six wide and three deep, 216 files. No downloaded
image and no `mkfs.erofs` on `PATH`, so the numbers below reproduce on
any machine.

## 2026-09-06 — before and after a metadata cache

| shape | reads | bytes | wall | reads/item |
|---|---:|---:|---:|---:|
| **uncached** | | | | |
| walk — list every directory (258 items) | 1952 | 3.54 MB | 5152 µs | 7.6 |
| stat — resolve 216 files by path | 1512 | 2.71 MB | 2890 µs | 7.0 |
| read — read 216 files | 1728 | 2.82 MB | 3016 µs | 8.0 |
| **512-block cache** | | | | |
| walk | 74 | 303 KB | 1184 µs | 0.3 |
| stat | **0** | 0 | 707 µs | 0.0 |
| read | **0** | 0 | 784 µs | 0.0 |

Reads fall by 96% on the walk and to nothing at all on the other two.

## Why there were two caches to think about, not one

This driver already had a cache when the measurement was taken —
`pcluster_cache`, an LRU of **decompressed pclusters**, keyed by
`(nid, blkaddr)`. It sits above the codec, so a hit there skips a
decompression as well as a read. It is the reason the `read` line is
not the expensive one.

What it never held is anything the codec does not touch: the
superblock, inodes, directory blocks, the xattr tables, the extent
maps. Every one of those went to the device each time it was wanted,
and resolving 216 paths re-read the same directories 216 times over.
That is the 1512 reads and 2.71 MB above.

`DEFAULT_METADATA_CACHE_BLOCKS` adds a second cache, keyed by device
block, underneath both. The pcluster cache's own misses are now served
from it when the blocks are still held.

## Why `stat` and `read` reach exactly zero

The fixture's metadata fits in 512 blocks — 2 MiB at a 4 KiB block
size. Once the walk has touched every directory, resolving any path
afterwards costs nothing, and reading a file costs nothing either
because its inode and extent map are metadata that the walk already
brought in and its data is small enough to have been cached alongside.

A larger image will not reach zero: the walk will evict as it goes, and
the figure to watch when that happens is `walk`, which is the shape
that touches the most distinct blocks. The 74 remaining reads there are
the first touch of each block, which nothing can remove.

## How to take the measurement again

```sh
cargo test --release --test read_path_cost -- --nocapture
```

It builds its own fixture, so it never skips.
