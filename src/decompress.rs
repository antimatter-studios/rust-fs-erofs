//! Codec wrappers for EROFS compressed clusters.
//!
//! Phase 3: LZ4, LZMA, and DEFLATE.
//!
//! On-disk format: EROFS stores raw codec output (no frame header).
//! - LZ4: raw BLOCK output, decoded via `lz4_flex::block::decompress_into`.
//! - LZMA: raw LZMA1 bitstream (NOT `.xz` framed). EROFS conveys the
//!   LZMA properties (lc, lp, pb, dict_size) out-of-band via the
//!   `z_erofs_lzma_cfgs` config block when
//!   `EROFS_FEATURE_INCOMPAT_COMPR_CFGS` is set; for v0.1 we assume
//!   the lzma-rs defaults. Real images that use non-default props
//!   need the parent zmap layer to plumb config through later.
//! - DEFLATE: raw DEFLATE blocks (no zlib/gzip wrapper), decoded with
//!   `flate2::Decompress::new(false)`.

use std::io::Cursor;

use flate2::{Decompress, FlushDecompress};

use crate::error::{Error, Result};
use crate::superblock::LzmaCfg;

/// EROFS compression-algorithm IDs. The `feature_incompat` SB bit
/// EROFS_FEATURE_INCOMPAT_ZERO_PADDING (0x1) and per-cluster advise
/// bits decide which arm gets dispatched; this enum is the
/// codec-layer surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Lz4 = 0,
    Lzma = 1,
    Deflate = 2,
}

impl Algorithm {
    pub fn from_id(id: u8) -> Result<Self> {
        match id {
            0 => Ok(Algorithm::Lz4),
            1 => Ok(Algorithm::Lzma),
            2 => Ok(Algorithm::Deflate),
            n => Err(Error::UnsupportedLayout(n)),
        }
    }
}

/// Decompress one cluster. `input` is the raw on-disk compressed
/// bytes; `output` must be sized to the EXACT decompressed length
/// (callers know this from the cluster geometry: cluster_size for
/// non-tail, residual for tail).
pub fn decompress(algo: Algorithm, input: &[u8], output: &mut [u8]) -> Result<()> {
    match algo {
        Algorithm::Lz4 => decompress_lz4(input, output),
        Algorithm::Lzma => decompress_lzma(input, output, &LzmaCfg::default()),
        Algorithm::Deflate => decompress_deflate(input, output),
    }
}

/// Like [`decompress`] but threads a per-codec config through (e.g.
/// the LZMA `dict_size` / `lc` / `lp` / `pb` parsed from the
/// COMPR_CFGS blob). Pass `None` to use the codec defaults — that's
/// equivalent to calling [`decompress`].
///
/// Why a separate entry point instead of always taking `Option`:
/// callers without an SB-level config (synthetic-image tests, the
/// mkfs writer paths) reach for the simpler API; only the read path
/// in `fs.rs` plumbs the parsed COMPR_CFGS through here.
pub fn decompress_with_config(
    algo: Algorithm,
    config: Option<&LzmaCfg>,
    input: &[u8],
    output: &mut [u8],
) -> Result<()> {
    match algo {
        Algorithm::Lz4 => decompress_lz4(input, output),
        Algorithm::Lzma => {
            let default = LzmaCfg::default();
            let cfg = config.unwrap_or(&default);
            decompress_lzma(input, output, cfg)
        }
        Algorithm::Deflate => decompress_deflate(input, output),
    }
}

fn decompress_lz4(input: &[u8], output: &mut [u8]) -> Result<()> {
    // Empty cluster: nothing to decode. EROFS never emits a zero-byte
    // compressed cluster against a non-empty output, so treat input
    // and output both being empty as a no-op.
    if input.is_empty() && output.is_empty() {
        return Ok(());
    }
    // EROFS_FEATURE_INCOMPAT_LZ4_0PADDING right-aligns the LZ4 frame
    // within the on-disk block, padding the LEADING bytes with zeros.
    // We don't plumb the feature bit down here; instead we skip any
    // leading zero bytes before calling the codec. lz4_flex's first
    // input byte is a token whose low 4 bits are the literal length;
    // a token of `0x00` would mean "literal_length=0, match_length=0"
    // which decodes nothing -- so a true compressed frame never
    // legitimately starts with a zero byte. Skipping leading zeros is
    // therefore lossless against non-padded inputs too.
    let inputmargin = input.iter().take_while(|&&b| b == 0).count();
    let real_input = &input[inputmargin..];
    if real_input.is_empty() {
        return Err(Error::BadInode("LZ4 input is all zeros"));
    }
    let written = lz4_flex::block::decompress_into(real_input, output)
        .map_err(|_| Error::BadInode("LZ4 decompression failed"))?;
    // Caller sized `output` to the exact decompressed length; a short
    // write means the compressed payload didn't expand to fill it.
    if written != output.len() {
        return Err(Error::BadInode("LZ4 decompressed size mismatch"));
    }
    Ok(())
}

fn decompress_lzma(input: &[u8], output: &mut [u8], cfg: &LzmaCfg) -> Result<()> {
    if input.is_empty() && output.is_empty() {
        return Ok(());
    }
    // EROFS right-aligns codec frames within the on-disk block (the
    // `EROFS_FEATURE_INCOMPAT_LZ4_0PADDING` design generalised to all
    // codecs). Skip any leading zero pad before handing the stream to
    // the codec.
    let inputmargin = input.iter().take_while(|&&b| b == 0).count();
    let real_input = &input[inputmargin..];
    if real_input.is_empty() {
        return Err(Error::BadInode("LZMA input is all zeros"));
    }

    // Two on-disk dialects coexist:
    // (a) Our writer emits the standard 13-byte LZMA1 header (5-byte
    //     properties + 8-byte uncompressed size) before the
    //     compressed stream; lzma-rs's default `lzma_decompress`
    //     reads this directly.
    // (b) mkfs.erofs strips the 13-byte header — properties live in
    //     the `z_erofs_lzma_cfgs` config block when
    //     `EROFS_FEATURE_INCOMPAT_COMPR_CFGS` is set. The caller
    //     parses the blob and passes the resulting `LzmaCfg`
    //     through; we synthesise a 13-byte header here from those
    //     values and feed lzma-rs.
    //
    // We try (a) first; on failure, fall back to (b). The standard
    // header sniffing is cheap and unambiguous because a valid LZMA1
    // properties byte lives in 0..=224 — different from the typical
    // values we'd see at the start of a stripped EROFS LZMA payload.
    //
    // Spec: LZMA1 header layout (general LZMA1 spec, not EROFS-
    // specific) and the `z_erofs_lzma_cfgs` struct in the public
    // format header `erofs_fs.h`.
    if try_decompress_lzma_with_header(real_input, output).is_ok() {
        return Ok(());
    }
    decompress_lzma_no_header(real_input, output, cfg)
}

fn try_decompress_lzma_with_header(input: &[u8], output: &mut [u8]) -> Result<()> {
    let mut decoded: Vec<u8> = Vec::with_capacity(output.len());
    let mut reader = Cursor::new(input);
    lzma_rs::lzma_decompress(&mut reader, &mut decoded)
        .map_err(|_| Error::BadInode("LZMA decompression failed"))?;
    if decoded.len() != output.len() {
        return Err(Error::BadInode("LZMA decompressed size mismatch"));
    }
    output.copy_from_slice(&decoded);
    Ok(())
}

fn decompress_lzma_no_header(input: &[u8], output: &mut [u8], cfg: &LzmaCfg) -> Result<()> {
    let mut framed: Vec<u8> = Vec::with_capacity(13 + input.len());
    // Properties byte = (pb * 5 + lp) * 9 + lc. LZMA1 imposes
    // 0 <= lc <= 8, 0 <= lp <= 4, 0 <= pb <= 4 (so the byte is
    // bounded by 0xE0). Reject out-of-range configs up front so
    // synthesised headers stay valid for lzma-rs.
    if cfg.lc > 8 || cfg.lp > 4 || cfg.pb > 4 {
        return Err(Error::BadInode("LZMA cfg lc/lp/pb out of range"));
    }
    let props = (cfg.pb as u16 * 5 + cfg.lp as u16) * 9 + cfg.lc as u16;
    framed.push(props as u8);
    framed.extend_from_slice(&cfg.dict_size.to_le_bytes());
    // Unpacked size: u64 LE. Provide exact expected length so the
    // decoder stops without an end-marker.
    framed.extend_from_slice(&(output.len() as u64).to_le_bytes());
    framed.extend_from_slice(input);

    let mut decoded: Vec<u8> = Vec::with_capacity(output.len());
    let mut reader = Cursor::new(framed);
    let opts = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::ReadHeaderButUseProvided(Some(
            output.len() as u64,
        )),
        memlimit: None,
        allow_incomplete: true,
    };
    lzma_rs::lzma_decompress_with_options(&mut reader, &mut decoded, &opts)
        .map_err(|_| Error::BadInode("LZMA decompression failed"))?;
    if decoded.len() != output.len() {
        return Err(Error::BadInode("LZMA decompressed size mismatch"));
    }
    output.copy_from_slice(&decoded);
    Ok(())
}

fn decompress_deflate(input: &[u8], output: &mut [u8]) -> Result<()> {
    if input.is_empty() && output.is_empty() {
        return Ok(());
    }
    // EROFS right-aligns codec frames within the on-disk block. Strip
    // any leading zero pad. A raw DEFLATE block header's BFINAL/BTYPE
    // bits are encoded in the LSBs of the first byte; for the first
    // block of a stream BTYPE=00 (stored) gives a first byte of 0x00,
    // which would collide with our zero-strip. mkfs.erofs never emits
    // BTYPE=00 (it always uses fixed/dynamic Huffman because stored
    // blocks don't compress), so a true frame's first byte is always
    // non-zero.
    let inputmargin = input.iter().take_while(|&&b| b == 0).count();
    let real_input = &input[inputmargin..];
    if real_input.is_empty() {
        return Err(Error::BadInode("DEFLATE input is all zeros"));
    }
    // `false` selects raw DEFLATE (no zlib header/checksum), which is
    // what EROFS stores.
    let mut decoder = Decompress::new(false);
    decoder
        .decompress(real_input, output, FlushDecompress::Finish)
        .map_err(|_| Error::BadInode("DEFLATE decompression failed"))?;
    if decoder.total_out() as usize != output.len() {
        return Err(Error::BadInode("DEFLATE decompressed size mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_from_id() {
        assert_eq!(Algorithm::from_id(0).unwrap(), Algorithm::Lz4);
        assert_eq!(Algorithm::from_id(1).unwrap(), Algorithm::Lzma);
        assert_eq!(Algorithm::from_id(2).unwrap(), Algorithm::Deflate);
        assert!(matches!(
            Algorithm::from_id(3),
            Err(Error::UnsupportedLayout(3))
        ));
        assert!(matches!(
            Algorithm::from_id(99),
            Err(Error::UnsupportedLayout(99))
        ));
    }

    #[test]
    fn round_trip_lz4() {
        let original = b"the quick brown fox jumps over the lazy dog. \
            the quick brown fox jumps over the lazy dog. \
            the quick brown fox jumps over the lazy dog.";
        let compressed = lz4_flex::block::compress(original);
        let mut output = vec![0u8; original.len()];
        decompress(Algorithm::Lz4, &compressed, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    // Empty input + empty output is the documented contract: nothing
    // to decode, output already correctly sized at zero, succeeds.
    #[test]
    fn empty_input_empty_output() {
        let mut output = [0u8; 0];
        decompress(Algorithm::Lz4, &[], &mut output).unwrap();
        decompress(Algorithm::Lzma, &[], &mut output).unwrap();
        decompress(Algorithm::Deflate, &[], &mut output).unwrap();
    }

    #[test]
    fn wrong_output_size_rejected() {
        let original = b"hello world, this is some test payload to compress";
        let compressed = lz4_flex::block::compress(original);
        let mut output = vec![0u8; original.len() + 50];
        assert!(decompress(Algorithm::Lz4, &compressed, &mut output).is_err());
    }

    #[test]
    fn round_trip_lzma() {
        let original = b"the quick brown fox jumps over the lazy dog. \
            the quick brown fox jumps over the lazy dog. \
            the quick brown fox jumps over the lazy dog.";
        let mut compressed: Vec<u8> = Vec::new();
        let mut reader = Cursor::new(&original[..]);
        lzma_rs::lzma_compress(&mut reader, &mut compressed).unwrap();
        let mut output = vec![0u8; original.len()];
        decompress(Algorithm::Lzma, &compressed, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    #[test]
    fn round_trip_deflate() {
        use flate2::{Compress, Compression, FlushCompress};
        let original = b"the quick brown fox jumps over the lazy dog. \
            the quick brown fox jumps over the lazy dog. \
            the quick brown fox jumps over the lazy dog.";
        // `false` = raw DEFLATE (no zlib wrapper), matching EROFS.
        let mut encoder = Compress::new(Compression::default(), false);
        let mut compressed = vec![0u8; original.len() + 64];
        encoder
            .compress(original, &mut compressed, FlushCompress::Finish)
            .unwrap();
        compressed.truncate(encoder.total_out() as usize);
        let mut output = vec![0u8; original.len()];
        decompress(Algorithm::Deflate, &compressed, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    // ---- error-path tests -------------------------------------------------
    //
    // Each codec wraps its underlying-library failure with
    // `Error::BadInode`. These tests pin that contract so a future
    // refactor can't quietly downgrade it (e.g. silently producing
    // garbage on a malformed frame).

    /// An LZ4 frame whose token byte is non-zero (so the leading-zero
    /// strip is a no-op) but whose payload doesn't decode. `0xFF`
    /// encodes "literal_length = 15 (extended)..." -- without the
    /// extension byte and 15 literal bytes following, the decoder
    /// rejects.
    #[test]
    fn decompress_lz4_rejects_malformed() {
        let bad = [0xFFu8, 0x01, 0x02, 0x03];
        let mut output = vec![0u8; 64];
        let err = decompress(Algorithm::Lz4, &bad, &mut output).unwrap_err();
        assert!(
            matches!(err, Error::BadInode(_)),
            "expected BadInode, got {err:?}"
        );
    }

    /// LZ4 input that's all zeros: rejected because we can't tell what
    /// real frame would have followed the pad.
    #[test]
    fn decompress_lz4_rejects_all_zeros() {
        let bad = [0u8; 16];
        let mut output = vec![0u8; 32];
        let err = decompress(Algorithm::Lz4, &bad, &mut output).unwrap_err();
        assert!(matches!(err, Error::BadInode(_)), "got {err:?}");
    }

    /// LZMA: a non-zero leading byte that isn't a valid properties
    /// byte, followed by garbage. Both the with-header and no-header
    /// paths must reject.
    #[test]
    fn decompress_lzma_rejects_malformed() {
        // 0xFF as a properties byte is invalid (max valid is 224); the
        // synthesised-header fallback also can't make sense of this.
        let bad = [0xFFu8; 64];
        let mut output = vec![0u8; 32];
        let err = decompress(Algorithm::Lzma, &bad, &mut output).unwrap_err();
        assert!(matches!(err, Error::BadInode(_)), "got {err:?}");
    }

    #[test]
    fn decompress_lzma_rejects_all_zeros() {
        let bad = [0u8; 32];
        let mut output = vec![0u8; 16];
        let err = decompress(Algorithm::Lzma, &bad, &mut output).unwrap_err();
        assert!(matches!(err, Error::BadInode(_)), "got {err:?}");
    }

    /// DEFLATE: a non-zero leading byte (so we don't trip the all-zero
    /// arm) but the bits don't form a valid block header.
    #[test]
    fn decompress_deflate_rejects_malformed() {
        // 0xFF then random; first byte's BFINAL=1, BTYPE=11 (reserved)
        // is rejected by every DEFLATE decoder.
        let bad = [0xFFu8, 0xFF, 0xFF, 0xFF];
        let mut output = vec![0u8; 32];
        let err = decompress(Algorithm::Deflate, &bad, &mut output).unwrap_err();
        assert!(matches!(err, Error::BadInode(_)), "got {err:?}");
    }

    #[test]
    fn decompress_deflate_rejects_all_zeros() {
        let bad = [0u8; 16];
        let mut output = vec![0u8; 8];
        let err = decompress(Algorithm::Deflate, &bad, &mut output).unwrap_err();
        assert!(matches!(err, Error::BadInode(_)), "got {err:?}");
    }

    /// `Algorithm::from_id` must reject any non-{0,1,2} id with an
    /// `UnsupportedLayout` carrying the offending byte.
    #[test]
    fn algorithm_from_id_rejects_unknown_99_and_255() {
        for id in [99u8, 255u8] {
            match Algorithm::from_id(id) {
                Err(Error::UnsupportedLayout(n)) => assert_eq!(n, id),
                other => panic!("id={id}: expected UnsupportedLayout, got {other:?}"),
            }
        }
    }

    /// Round-trips at >= 4 KiB exercise the "input padding edge cases"
    /// each codec calls out (LZ4 right-aligns within a block; DEFLATE
    /// and LZMA strip leading zero pad). The 4 KiB threshold keeps the
    /// payload large enough for each encoder to emit multi-byte
    /// references / dictionary lookups, not just the trivial "all
    /// literals" path the tiny round-trips above hit.
    #[test]
    fn round_trip_lz4_large_payload() {
        // Mix of a deterministic LCG + a literal tail so the encoder
        // can't trivially compress it to nothing but still finds enough
        // repetition for matches.
        let mut data = Vec::with_capacity(8192);
        let mut x: u32 = 0xDEAD_BEEF;
        for _ in 0..8192 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            data.push((x >> 16) as u8);
        }
        for _ in 0..1024 {
            data.extend_from_slice(b"abcdef");
        }
        let compressed = lz4_flex::block::compress(&data);
        let mut output = vec![0u8; data.len()];
        decompress(Algorithm::Lz4, &compressed, &mut output).unwrap();
        assert_eq!(output, data);
    }

    #[test]
    fn round_trip_lzma_large_payload() {
        let mut data = Vec::with_capacity(8192);
        for i in 0..8192 {
            data.push(((i * 31) & 0xFF) as u8);
        }
        let mut compressed: Vec<u8> = Vec::new();
        let mut reader = Cursor::new(&data[..]);
        lzma_rs::lzma_compress(&mut reader, &mut compressed).unwrap();
        let mut output = vec![0u8; data.len()];
        decompress(Algorithm::Lzma, &compressed, &mut output).unwrap();
        assert_eq!(output, data);
    }

    #[test]
    fn round_trip_deflate_large_payload() {
        use flate2::{Compress, Compression, FlushCompress};
        let mut data = Vec::with_capacity(8192);
        for i in 0..8192 {
            data.push(((i * 17 + 5) & 0xFF) as u8);
        }
        let mut encoder = Compress::new(Compression::default(), false);
        let mut compressed = vec![0u8; data.len() + 1024];
        encoder
            .compress(&data, &mut compressed, FlushCompress::Finish)
            .unwrap();
        compressed.truncate(encoder.total_out() as usize);
        let mut output = vec![0u8; data.len()];
        decompress(Algorithm::Deflate, &compressed, &mut output).unwrap();
        assert_eq!(output, data);
    }

    /// LZ4 payload with leading zero pad simulates the
    /// `EROFS_FEATURE_INCOMPAT_LZ4_0PADDING` right-alignment we
    /// document in the codec body. Round-trips through the strip-pad
    /// path that the basic round_trip_lz4 doesn't exercise.
    #[test]
    fn round_trip_lz4_with_leading_zero_pad() {
        let original = b"some data to compress and round trip back through the codec";
        let mut padded = vec![0u8; 16];
        padded.extend_from_slice(&lz4_flex::block::compress(original));
        let mut output = vec![0u8; original.len()];
        decompress(Algorithm::Lz4, &padded, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    /// Same right-alignment idea, applied to LZMA. The codec body
    /// claims it strips leading zeros for every codec; pin it.
    #[test]
    fn round_trip_lzma_with_leading_zero_pad() {
        let original = b"lzma payload that's long enough to actually compress";
        let mut compressed: Vec<u8> = Vec::new();
        let mut reader = Cursor::new(&original[..]);
        lzma_rs::lzma_compress(&mut reader, &mut compressed).unwrap();
        let mut padded = vec![0u8; 8];
        padded.extend_from_slice(&compressed);
        let mut output = vec![0u8; original.len()];
        decompress(Algorithm::Lzma, &padded, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    /// Encode a payload with lzma-rs and reshape its output into the
    /// EROFS on-disk LZMA dialect: bare bitstream (no LZMA1 13-byte
    /// header), with the LZMA1 init "discard byte" replaced by a
    /// non-zero placeholder so the leading-zero strip in
    /// `decompress_lzma` doesn't eat it.
    ///
    /// The LZMA1 RangeDecoder always reads-and-discards the first
    /// byte of the bitstream (per the LZMA1 spec). lzma-rs's encoder
    /// writes that byte as 0x00; mkfs.erofs writes whatever the range
    /// coder produces (typically non-zero for non-trivial inputs).
    /// EROFS's on-disk pad pattern (leading zeros) plus our reader's
    /// leading-zero strip would eat lzma-rs's 0x00 discard byte if
    /// fed verbatim, shifting the subsequent `code` window by one
    /// byte and decoding garbage. We sidestep this by skipping the
    /// 0x00 (`framed[14..]`) and prepending an arbitrary non-zero
    /// placeholder so the strip leaves the byte alone and the
    /// RangeDecoder correctly reads-and-discards it. The 4 bytes that
    /// follow are then the same 4 bytes lzma-rs would have read as
    /// the initial `code`.
    fn build_erofs_bare_lzma_payload(original: &[u8]) -> (Vec<u8>, u32) {
        let mut framed: Vec<u8> = Vec::new();
        let mut reader = Cursor::new(original);
        lzma_rs::lzma_compress(&mut reader, &mut framed).unwrap();
        // Byte 13 is the LZMA1 init "discard byte" (0x00); we replace
        // it with 0x01 so the leading-zero strip leaves it intact and
        // RangeDecoder still discards exactly one byte.
        assert_eq!(framed[13], 0x00, "lzma-rs init byte should be 0x00");
        let mut bare = vec![0x01u8];
        bare.extend_from_slice(&framed[14..]);
        let dict_size = u32::from_le_bytes(framed[1..5].try_into().unwrap());
        (bare, dict_size)
    }

    /// LZMA round-trip with a non-default `dict_size` plumbed via
    /// `decompress_with_config`. lzma-rs's encoder defaults to a
    /// dict_size of 8 MiB; the bitstream itself uses match distances
    /// that fit within the dict the encoder advertised. We pass the
    /// SAME dict_size back through the config (proving the plumbing
    /// works without changing semantics).
    #[test]
    fn lzma_with_non_default_dict_size_round_trip() {
        let original = b"lzma config-aware round trip with a non-default dict_size value!\
            lzma config-aware round trip with a non-default dict_size value!";
        let (bare, encoded_dict_size) = build_erofs_bare_lzma_payload(original);
        // Pick a dict_size that's NOT the LzmaCfg default (1<<24 = 16
        // MiB) and matches what the encoder used. This exercises the
        // cfg-driven header synthesis: the synthesized header carries
        // OUR cfg.dict_size, not the codec default.
        assert_ne!(
            encoded_dict_size,
            LzmaCfg::default().dict_size,
            "encoder dict differs from default"
        );
        let cfg = LzmaCfg {
            dict_size: encoded_dict_size,
            lc: 3,
            lp: 0,
            pb: 2,
        };
        let mut output = vec![0u8; original.len()];
        decompress_with_config(Algorithm::Lzma, Some(&cfg), &bare, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    /// Same idea but pins the lc / lp / pb plumbing. lzma-rs's
    /// encoder always emits (lc=3, lp=0, pb=2), so we feed those
    /// EXACT values back through the config; if our props-byte
    /// derivation `(pb*5 + lp)*9 + lc` is wrong, the synthesised
    /// header's props byte would differ from what the encoder used
    /// and decode would produce garbage / fail.
    #[test]
    fn lzma_with_custom_props_round_trip() {
        let original = b"check lc/lp/pb propagation. \
            check lc/lp/pb propagation. check lc/lp/pb propagation.";
        let (bare, encoded_dict_size) = build_erofs_bare_lzma_payload(original);
        let cfg = LzmaCfg {
            dict_size: encoded_dict_size,
            lc: 3,
            lp: 0,
            pb: 2,
        };
        let mut output = vec![0u8; original.len()];
        decompress_with_config(Algorithm::Lzma, Some(&cfg), &bare, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }

    /// Out-of-range lc/lp/pb config values are rejected before the
    /// codec sees them. Pin the contract so a caller can't sneak a
    /// malformed config through.
    #[test]
    fn lzma_with_out_of_range_props_rejected() {
        let cfg = LzmaCfg {
            dict_size: 1 << 20,
            lc: 9, // > 8
            lp: 0,
            pb: 2,
        };
        let mut output = vec![0u8; 16];
        let err = decompress_with_config(Algorithm::Lzma, Some(&cfg), &[0xFFu8; 32], &mut output)
            .unwrap_err();
        assert!(matches!(err, Error::BadInode(_)), "got {err:?}");
    }

    /// And for DEFLATE.
    #[test]
    fn round_trip_deflate_with_leading_zero_pad() {
        use flate2::{Compress, Compression, FlushCompress};
        let original = b"deflate payload long enough to compress with some redundancy redundancy";
        let mut encoder = Compress::new(Compression::default(), false);
        let mut compressed = vec![0u8; original.len() + 64];
        encoder
            .compress(original, &mut compressed, FlushCompress::Finish)
            .unwrap();
        compressed.truncate(encoder.total_out() as usize);
        let mut padded = vec![0u8; 4];
        padded.extend_from_slice(&compressed);
        let mut output = vec![0u8; original.len()];
        decompress(Algorithm::Deflate, &padded, &mut output).unwrap();
        assert_eq!(&output[..], &original[..]);
    }
}
