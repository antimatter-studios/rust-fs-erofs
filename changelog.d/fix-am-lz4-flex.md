### Fixed

- **The crate can be published again (#269).** Its LZ4HC encoder came from a
  git dependency on a fork of lz4_flex, which crates.io refuses, so no
  release could be made after #252. The encoder now comes from that fork
  published as `am-lz4-flex` 0.14.0, pinned exactly, until upstream releases
  PSeitz/lz4_flex#244. Its fixed-output blocks are byte-identical to the
  fork commit used before, so the images built do not change.
