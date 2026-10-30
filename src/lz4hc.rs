//! LZ4HC, through liblz4.
//!
//! The writer's high-ratio LZ4 encoder. `mkfs.erofs -zlz4hc` calls
//! liblz4's `LZ4_compress_HC_destSize`, which compresses as much of its
//! input as fits in a fixed output: exactly what filling a physical
//! cluster needs. No pure-Rust LZ4 encoder has an HC mode or a
//! fixed-output one -- `lz4_flex`, which this crate decodes with, has
//! neither -- so this is the reference implementation itself, which
//! `lz4-sys` (MIT) builds from liblz4 1.10's BSD-2 `lib/` sources. Its
//! output is standard LZ4 blocks, which the kernel and `lz4_flex` decode.
//!
//! `lz4-sys` binds liblz4's frame and fast-block functions but not these
//! two, so they are declared here; the library it links defines them.

use lz4_sys as _;
use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    fn LZ4_sizeofStateHC() -> c_int;
    fn LZ4_compress_HC_destSize(
        state: *mut c_void,
        src: *const c_char,
        dst: *mut c_char,
        src_size: *mut c_int,
        target_dst_size: c_int,
        level: c_int,
    ) -> c_int;
}

/// An LZ4HC encoder and the working state it reuses across calls.
pub(crate) struct Lz4Hc {
    /// `LZ4_streamHC_t`, which wants pointer alignment; `u64` gives it.
    state: Vec<u64>,
    level: c_int,
}

impl Lz4Hc {
    pub(crate) fn new(level: u8) -> Self {
        // SAFETY: a pure size query.
        let bytes = unsafe { LZ4_sizeofStateHC() } as usize;
        Self {
            state: vec![0u64; bytes.div_ceil(8)],
            level: level as c_int,
        }
    }

    /// Compress as much of `src` as fits in `dst`, as one LZ4 block that
    /// decodes on its own. Returns the input bytes it took and the output
    /// bytes it wrote; `(0, 0)` when not even one byte fits.
    pub(crate) fn compress_dest_size(&mut self, src: &[u8], dst: &mut [u8]) -> (usize, usize) {
        let mut taken = src.len().min(c_int::MAX as usize) as c_int;
        let target = dst.len().min(c_int::MAX as usize) as c_int;
        // SAFETY: `state` is at least `LZ4_sizeofStateHC()` bytes and
        // 8-aligned, and the call initialises it; `src` is valid for
        // `taken` bytes and `dst` for `target`, and liblz4 reads and
        // writes no further. It writes back in `taken` how much of `src`
        // it consumed.
        let written = unsafe {
            LZ4_compress_HC_destSize(
                self.state.as_mut_ptr().cast(),
                src.as_ptr().cast(),
                dst.as_mut_ptr().cast(),
                &mut taken,
                target,
                self.level,
            )
        };
        if written <= 0 {
            return (0, 0);
        }
        (taken as usize, written as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_output_takes_more_input_than_it_holds_and_decodes_back() {
        // Words in an order that never quite repeats, so 32 KiB of output
        // holds a few times that of input and no more.
        let mut state = 1u64;
        let src: Vec<u8> = (0..200_000)
            .flat_map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                format!("w{} ", state % 4096).into_bytes()
            })
            .collect();
        let mut dst = vec![0u8; 32 << 10];
        let (taken, written) = Lz4Hc::new(12).compress_dest_size(&src, &mut dst);
        assert!(
            written <= dst.len() && written > dst.len() - 64,
            "{written}"
        );
        assert!(
            taken > written && taken < src.len(),
            "took {taken} for {written}"
        );
        let back = lz4_flex::block::decompress(&dst[..written], taken).unwrap();
        assert_eq!(back, src[..taken]);
    }

    #[test]
    fn input_smaller_than_the_output_is_taken_whole() {
        let src = b"short and sweet, short and sweet, short and sweet".to_vec();
        let mut dst = vec![0u8; 4096];
        let (taken, written) = Lz4Hc::new(12).compress_dest_size(&src, &mut dst);
        assert_eq!(taken, src.len());
        assert_eq!(
            lz4_flex::block::decompress(&dst[..written], taken).unwrap(),
            src
        );
    }
}
