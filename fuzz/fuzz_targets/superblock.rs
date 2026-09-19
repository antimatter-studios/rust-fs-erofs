#![no_main]
//! The superblock, read before anything is known. `blkszbits` is a
//! shift, and the block size it produces divides and multiplies
//! everything read afterwards.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_erofs::Superblock::parse(data);
});
