//! LZ4HC, through the antimatter-studios fork of `lz4_flex`.
//!
//! The writer's high-ratio LZ4 encoder. A physical cluster needs a
//! fixed-output mode, which compresses as much of its input as fits, and
//! the fork adds that as `compress_hc_dest_size` over upstream's open HC
//! work. Its output is standard LZ4 blocks, which the kernel and `lz4_flex`
//! decode.

/// An LZ4HC encoder at one compression level.
pub(crate) struct Lz4Hc {
    level: u8,
}

impl Lz4Hc {
    pub(crate) fn new(level: u8) -> Self {
        Self { level }
    }

    /// Compress as much of `src` as fits in `dst`, as one LZ4 block that
    /// decodes on its own. Returns the input bytes it took and the output
    /// bytes it wrote; `(0, 0)` when not even one byte fits.
    pub(crate) fn compress_dest_size(&mut self, src: &[u8], dst: &mut [u8]) -> (usize, usize) {
        lz4_flex_hc::block::compress_hc_dest_size(src, dst, self.level)
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
