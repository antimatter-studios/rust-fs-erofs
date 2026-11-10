### Changed

- The builder uses the faster LZ4HC encode path and a level-12 search budget
  scaled to cluster capacity, with benchmark checks guarding compressed output
  size against liblz4.
