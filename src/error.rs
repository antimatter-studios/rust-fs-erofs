use fs_core::Error as BlockError;

#[derive(Debug)]
pub enum Error {
    /// Underlying block device returned an error.
    Block(BlockError),
    /// Bytes at the EROFS_SUPER_OFFSET don't carry the magic number.
    NotErofs,
    /// Superblock parse rejected the on-disk values (impossible block size,
    /// truncated, etc.). The string carries a short reason for diagnostics.
    BadSuperblock(&'static str),
    /// Inode at the requested NID is malformed or its declared layout
    /// is one Phase 0 doesn't implement (compression, chunked).
    BadInode(&'static str),
    /// Layout requested isn't supported in this build. Compression layouts
    /// (1, 3) and chunk-based (4) all surface as this in Phase 0.
    UnsupportedLayout(u8),
    /// A directory block didn't pass dirent-array sanity checks.
    BadDirent(&'static str),
    /// An xattr inline area didn't pass header / entry sanity checks.
    BadXattr(&'static str),
    /// Lookup for a name that isn't present in a directory.
    NotFound,
    /// Path component traversal hit a non-directory.
    NotADirectory,
    /// Read past the end of a file.
    OutOfRange,
}

impl From<BlockError> for Error {
    fn from(e: BlockError) -> Self {
        Error::Block(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Block(e) => write!(f, "block device: {e:?}"),
            Error::NotErofs => write!(f, "not an EROFS image (magic mismatch at byte 1024)"),
            Error::BadSuperblock(s) => write!(f, "malformed superblock: {s}"),
            Error::BadInode(s) => write!(f, "malformed inode: {s}"),
            Error::UnsupportedLayout(n) => {
                write!(
                    f,
                    "data layout {n} not supported in Phase 0 (compression/chunked)"
                )
            }
            Error::BadDirent(s) => write!(f, "malformed directory: {s}"),
            Error::BadXattr(s) => write!(f, "malformed xattr: {s}"),
            Error::NotFound => write!(f, "not found"),
            Error::NotADirectory => write!(f, "not a directory"),
            Error::OutOfRange => write!(f, "read past end of file"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    /// One assert per variant: every `Display` arm is exercised, and the
    /// substring check pins the documented contract (e.g. "magic mismatch"
    /// for `NotErofs`) without forcing the whole message to stay verbatim.
    #[test]
    fn display_block() {
        let e = Error::Block(BlockError::ShortRead {
            offset: 10,
            want: 4,
            got: 0,
        });
        let s = e.to_string();
        assert!(s.contains("block device"), "got: {s}");
    }

    #[test]
    fn display_not_erofs() {
        assert!(Error::NotErofs.to_string().contains("magic mismatch"));
    }

    #[test]
    fn display_bad_superblock() {
        let s = Error::BadSuperblock("blkszbits out of range").to_string();
        assert!(s.contains("malformed superblock"));
        assert!(s.contains("blkszbits out of range"));
    }

    #[test]
    fn display_bad_inode() {
        let s = Error::BadInode("buffer shorter than 32 bytes").to_string();
        assert!(s.contains("malformed inode"));
        assert!(s.contains("buffer shorter than 32 bytes"));
    }

    #[test]
    fn display_unsupported_layout() {
        let s = Error::UnsupportedLayout(7).to_string();
        assert!(s.contains("data layout 7"));
    }

    #[test]
    fn display_bad_dirent() {
        let s = Error::BadDirent("nameoff overflow").to_string();
        assert!(s.contains("malformed directory"));
        assert!(s.contains("nameoff overflow"));
    }

    #[test]
    fn display_bad_xattr() {
        let s = Error::BadXattr("entry truncated").to_string();
        assert!(s.contains("malformed xattr"));
        assert!(s.contains("entry truncated"));
    }

    #[test]
    fn display_not_found() {
        assert!(Error::NotFound.to_string().contains("not found"));
    }

    #[test]
    fn display_not_a_directory() {
        assert!(Error::NotADirectory.to_string().contains("not a directory"));
    }

    #[test]
    fn display_out_of_range() {
        assert!(Error::OutOfRange
            .to_string()
            .contains("read past end of file"));
    }

    /// `From<BlockError>` is the gateway used at every `?` against a
    /// `BlockRead` call -- verify it produces the `Block` variant, not
    /// some other arm.
    #[test]
    fn from_block_error_wraps_in_block_variant() {
        let block_err = BlockError::ShortRead {
            offset: 1024,
            want: 128,
            got: 0,
        };
        let err: Error = block_err.into();
        match err {
            Error::Block(inner) => match inner {
                BlockError::ShortRead { offset, want, got } => {
                    assert_eq!(offset, 1024);
                    assert_eq!(want, 128);
                    assert_eq!(got, 0);
                }
                other => panic!("inner was not ShortRead: {other:?}"),
            },
            other => panic!("expected Error::Block, got {other:?}"),
        }
    }

    /// Compile-only check that `Error: std::error::Error` -- the trait
    /// bound is exercised at type-check time, not at run time. Done via
    /// a `&dyn` upcast so the bound is a hard requirement.
    #[test]
    fn implements_std_error_trait() {
        let err = Error::NotErofs;
        let _: &dyn std::error::Error = &err;
    }
}
