### Added

- **The builder shares repeated xattrs (#254).** With
  `BuildOptions::share_xattrs`, an xattr two or more inodes carry -- the
  same namespace, name and value -- is written once in the shared xattr
  area, and each of those inodes holds a 4-byte reference to it, as
  `mkfs.erofs` does. Off by default, so existing callers build the same
  bytes.
