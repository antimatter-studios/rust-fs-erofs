//! Multi-megabyte file round-trips. Uses a deterministic PRNG so the
//! test is reproducible without storing megabytes of expected bytes.
//!
//! The 100 MB case is `#[ignore]`-gated -- it allocates ~300 MB peak
//! (plaintext + image + read-back buffer) and would slow down a fast
//! `cargo test` cycle.

mod common;

use common::{dir, file, open_image};
use fs_erofs::mkfs;

/// xorshift64 PRNG -- deterministic, keeps the test cheap.
fn fill_pseudo_random(buf: &mut [u8], seed: u64) {
    let mut x = if seed == 0 {
        0xdead_beef_cafe_babe
    } else {
        seed
    };
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = bytes[i];
        }
    }
}

/// Build an image of one file of size `n_bytes`, read it all back,
/// and verify byte-for-byte equality.
fn round_trip_size(n_bytes: usize, seed: u64) {
    let mut payload = vec![0u8; n_bytes];
    fill_pseudo_random(&mut payload, seed);

    let img = mkfs::build_image(dir(vec![("big.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/big.bin").unwrap();
    assert_eq!(inode.size as usize, n_bytes);
    let mut buf = vec![0u8; n_bytes];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf.len(), payload.len());
    // Use chunked compare so failure messages don't dump megabytes.
    for (i, (a, b)) in buf.iter().zip(payload.iter()).enumerate() {
        assert_eq!(a, b, "byte {i} differs");
    }
}

#[test]
fn one_mb_round_trip() {
    // `1 * 1024 * 1024` parallels the `8 * 1024 * 1024` /
    // `100 * 1024 * 1024` neighbours below; keep the leading factor for
    // readability rather than collapsing to a bare `1024 * 1024`.
    #[allow(clippy::identity_op)]
    let n = 1 * 1024 * 1024;
    round_trip_size(n, 0x1111_2222_3333_4444);
}

#[test]
fn eight_mb_round_trip() {
    round_trip_size(8 * 1024 * 1024, 0xaaaa_bbbb_cccc_dddd);
}

#[test]
#[ignore = "100 MB allocation is slow under default cargo test; opt in with --ignored"]
fn one_hundred_mb_round_trip() {
    round_trip_size(100 * 1024 * 1024, 0x9999_8888_7777_6666);
}

/// Verify partial reads at arbitrary offsets land on the right bytes.
#[test]
fn partial_reads_at_offsets() {
    // 1 MiB; spelled `1 * 1024 * 1024` so it scans like the other MB-sized
    // round-trip sizes in this file.
    #[allow(clippy::identity_op)]
    let n_bytes = 1 * 1024 * 1024;
    let mut payload = vec![0u8; n_bytes];
    fill_pseudo_random(&mut payload, 0xfeed_face_cafe_babe);
    let img = mkfs::build_image(dir(vec![("big.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/big.bin").unwrap();

    // Spot-check at block boundaries and mid-block.
    for &off in &[0u64, 1, 4095, 4096, 4097, 100_000, 1_048_575] {
        let len = 256usize.min(n_bytes - off as usize);
        let mut buf = vec![0u8; len];
        fs.read_file(&inode, off, &mut buf).unwrap();
        assert_eq!(
            buf,
            &payload[off as usize..off as usize + len],
            "offset {off}",
        );
    }
}
