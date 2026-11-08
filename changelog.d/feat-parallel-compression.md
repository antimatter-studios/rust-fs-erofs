### Added

- **The builder compresses on every core (#262).** `BuildOptions::threads`
  sets how many threads compress, 0 for one per core, the default. Each
  file is compressed in 4 MiB segments that start fresh pclusters, read
  once in order and written in order, so any thread count builds the same
  bytes; a segment boundary costs under 0.1% of image size. Each thread adds
  about 8 MiB of memory; with one thread nothing is spawned.
