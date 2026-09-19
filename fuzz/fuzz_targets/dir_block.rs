#![no_main]
//! A directory block. Entry names are bounded by where the *next*
//! entry's name begins, so one wrong `nameoff` reads the name of an
//! entry that is not there.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_erofs::dir::iter_block(data);
    let _ = fs_erofs::dir::visit_block(data, |_, _, _| {});
});
