### Added

- **`XattrSpec::from_full_name` builds an xattr from a full name such as
  `security.selinux` or `user.foo` (#263),** taking the namespace index
  from the same prefix table the reader resolves names with, and
  `xattr::split_full_name` is that inverse of `resolve_full_name`. The
  POSIX ACL names map to their indices with an empty name; a name with no
  known prefix is refused with `BadXattr`, not kept whole under the raw
  namespace.

### Changed

- **`Content` and `Node` take a lifetime, `Content<'a>` and `Node<'a>`
  (#263),** for which a file's opener and reader may borrow -- files read
  out of another open image, say -- and neither needs to be `Send`:
  content is only read on the thread that builds the image, whether or
  not other threads compress it. `build_image`, `build_image_with` and
  `build_image_to` take a `Node` of any lifetime. A function that names
  `Node` in its return type now names its lifetime, `Node<'static>` for
  a tree that owns everything; nothing else changes for a caller.
