#![no_main]
//! The format field selects the layout, so one entry point is really
//! several decoders: compact and extended, and a datalayout that says
//! whether the data is plain, inline, compressed or chunked.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fs_erofs::Inode::parse(1, data);
});
