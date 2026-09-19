#![no_main]
//! A whole image, opened and walked.
//!
//! This is the target with the most reach: opening the superblock,
//! reading the root inode, listing a directory, resolving xattrs,
//! following a symlink and reading a file between them cover `zmap`,
//! `chunked`, `decompress` and the xattr readers, none of which take a
//! plain byte slice and so cannot be fuzzed directly.
//!
//! EROFS is read-only and mounted from sources the reader did not
//! produce -- a container image, an appliance, an OTA payload -- which
//! is exactly the threat model a fuzzer is for.
use fs_erofs_fuzz::walk;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    walk(data);
});
