//! Pure-Rust EROFS (Enhanced Read-Only File System) reader.
//!
//! Reads everything `mkfs.erofs` 1.9 emits: compact and extended
//! inodes, FLAT_PLAIN and FLAT_INLINE, chunk-based inodes, and
//! compressed clusters in LZ4, LZMA, DEFLATE and ZSTD — including
//! compacted-2B cluster maps, ztailpacking, fragments and
//! big_pcluster. It also builds images; see [`mkfs`].
//!
//! This block used to say "Phase 0 scope — uncompressed images only",
//! with compressed and chunked inodes returning
//! `Error::UnsupportedLayout`. That stopped being true several
//! thousand lines of `zmap.rs` and `chunked.rs` ago, and `Cargo.toml`'s
//! own description already contradicted it.
//!
//! Generate a test image with `mkfs.erofs` (no `-z` flag = no
//! compression):
//!
//! ```sh
//! mkfs.erofs out.img source-tree/
//! ```
//!
//! Open it via [`Filesystem::open`] over any [`fs_core::BlockRead`] —
//! `FileDevice` for a path, `SliceReader` for an in-memory buffer.
//!
//! Spec: `linux/fs/erofs/erofs_fs.h` and `Documentation/filesystems/
//! erofs.rst`. Field names mirror the kernel struct names with the
//! `i_` / `s_` prefixes dropped where redundant.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod acl;
pub mod chunked;
pub mod decompress;
pub mod dir;
pub mod error;
pub mod fs;
pub mod inode;
pub mod layout;
pub mod mkfs;
pub mod superblock;
/// One in-memory device for every test in this crate.
#[cfg(test)]
pub(crate) mod test_device;
pub mod xattr;
pub mod zmap;

// C ABI exports — surface defined in `include/fs_erofs.h`.
pub mod capi;

pub use acl::{AclEntry, AclPerm, AclTag};
pub use chunked::{ChunkInfo, EROFS_NULL_ADDR};
pub use decompress::{decompress, decompress_with_config, Algorithm};
pub use dir::{DirEntry, EROFS_DIRENT_SIZE};
pub use error::{Error, Result};
pub use fs::Filesystem;
pub use inode::{FileType, Inode};
pub use layout::{DataLayout, InodeFormat, InodeVersion};
pub use superblock::{
    read_compr_cfgs, ComprCfgs, LzmaCfg, Superblock, EROFS_FEATURE_COMPAT_SB_CHKSUM,
    EROFS_FEATURE_INCOMPAT_COMPR_CFGS, EROFS_SUPER_MAGIC_V1, EROFS_SUPER_OFFSET,
};
pub use xattr::{
    parse_inline_xattrs, read_all_xattrs, read_inline_xattrs, read_shared_xattrs,
    read_xattr_prefix_dictionary, resolve_full_name, resolve_with_dict, XattrEntry,
    XattrLongPrefix, EROFS_XATTR_LONG_PREFIX, EROFS_XATTR_LONG_PREFIX_MASK,
};
pub use zmap::{ClusterMapping, ZMap};

// DOES THIS BUILD ACTUALLY TRAP AN ARITHMETIC OVERFLOW?
//
// Inline in `lib.rs` rather than a module of its own under `src/`: a
// separate file hangs off one `mod` line, and losing that line leaves
// the file present, uncompiled and asserting nothing, with no lint to
// say so. That has already happened once on a sibling repository's
// version of this fix -- a `git reset --hard` took the declaration, the
// file stayed, and seven assertions quietly stopped existing. Inline,
// there is no declaration to lose. It cannot live in `tests/` either:
// the question it answers is about the library target that the debug
// step in ci.yml builds, so it has to be part of that target.
#[cfg(test)]
mod overflow_checks {
    /// Set by the debug step in `ci.yml`, and by nothing else.
    ///
    /// The release steps must NOT set it: overflow checks are off there
    /// deliberately, because that is what ships. Setting it on a
    /// release run makes this module fail every time, which is the
    /// correct and loud response to that misconfiguration.
    const HANDSHAKE: &str = "EXPECT_OVERFLOW_CHECKS";

    /// Perform an overflow and report whether the program was stopped.
    ///
    /// The only question that matters, and the only one a text scan of
    /// `Cargo.toml` or the workflow cannot answer on its own: whichever
    /// spelling of "the checks are off" might exist -- a manifest key
    /// in any of its several spellings, a `CARGO_PROFILE_*` variable at
    /// step or job level, a `.cargo/config.toml` -- this asks the build
    /// directly instead of enumerating them and hoping the list is
    /// complete.
    fn this_build_traps_an_overflow() -> bool {
        // The hook is silenced so a deliberate panic does not print a
        // scary backtrace into a passing job's log.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let trapped = std::panic::catch_unwind(|| {
            let big = std::hint::black_box(u64::MAX);
            std::hint::black_box(big + 1);
        })
        .is_err();
        std::panic::set_hook(previous);
        trapped
    }

    /// When the gate says it built a profile that traps, check that it
    /// did.
    ///
    /// With `HANDSHAKE` unset this asserts nothing -- the shape of a
    /// test that passes because its fixture is missing -- and that is
    /// not guarded here because it cannot be: a build has no way to
    /// know whether it was supposed to be the checking one. It is
    /// guarded in `tests/ci_profile.rs`, which reads `ci.yml` and
    /// refuses if no `cargo test` there runs without `--release` while
    /// setting this variable.
    #[test]
    fn the_build_the_gate_asked_to_check_does_check() {
        let asked = match std::env::var(HANDSHAKE) {
            Ok(value) if !value.is_empty() => value,
            _ => return,
        };

        assert!(
            this_build_traps_an_overflow(),
            "{HANDSHAKE}={asked} was set, so this run is the one that is \
             supposed to panic on arithmetic overflow -- and it did not. \
             The debug step is running and blind, which is the exact \
             state it exists to rule out."
        );
    }
}
