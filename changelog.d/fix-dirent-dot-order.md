### Fixed

- **A directory block holds "." and ".." in byte order among its other
  names (#258).** The builder put them first, so a directory holding a
  name that sorts before "." -- `+x`, `-x`, `,x`, as Android trees have
  -- was refused by `fsck.erofs` ("wrong dirent name order") and could not
  be searched by the kernel.
