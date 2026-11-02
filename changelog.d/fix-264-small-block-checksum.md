### Fixed

- An image of 1 KiB blocks or less no longer carries a superblock checksum, as `mkfs.erofs` writes none there. `fsck.erofs` 1.8.6 crashed on a 512-byte image that had one. The reader still verifies images from older tools that do.
