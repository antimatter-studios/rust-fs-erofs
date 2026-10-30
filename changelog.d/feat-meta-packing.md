### Changed

- **An inode with an inline tail fills the room an earlier metadata block
  was left with (#259).** An inode and its tail must share one block, and
  the builder placed each one after the last, starting a new block when
  it did not fit, so a tree of small files wasted up to a block apiece.
  It now puts each one in the block with the least room that still holds
  it, as `mkfs.erofs` does: on about 2600 small files its image went from
  6% larger than `mkfs.erofs -zlz4hc,12 -C 32768`'s to 0.08%.
