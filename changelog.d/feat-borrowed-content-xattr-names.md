### Added

- **`XattrSpec::from_full_name` builds an xattr from a full name such as
  `security.selinux` or `user.foo` (#263),** taking the namespace index
  from the same prefix table the reader resolves names with, and
  `xattr::split_full_name` is that inverse of `resolve_full_name`. The
  POSIX ACL names map to their indices with an empty name; a name with no
  known prefix is refused with `BadXattr`, not kept whole under the raw
  namespace.
