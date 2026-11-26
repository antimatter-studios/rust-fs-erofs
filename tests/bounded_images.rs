//! The helpers the suite looks at images through (#302): they see a changed
//! byte wherever it is, past the first buffer included, and they refuse to
//! read a large file whole.

use fs_erofs_test_support::{
    assert_file_bytes, assert_image_unchanged, copy_sparse_at, first_difference, read_image_region,
    read_small_file, sha256_file, sha256_hex, snapshot_image, write_image_region, ScratchDir,
    SMALL_FILE,
};

/// Longer than two buffers, with holes and with data on both sides of each.
fn image() -> Vec<u8> {
    let mut bytes = vec![0u8; (5 << 20) / 2 + 123];
    for at in [0, 4095, 1 << 20, (1 << 20) + 5, (2 << 20) + 17] {
        bytes[at] = 0xA5;
    }
    let last = bytes.len() - 1;
    bytes[last] = 7;
    bytes
}

#[test]
fn a_snapshot_matches_until_a_byte_past_the_first_buffer_changes() {
    let dir = ScratchDir::new("bounded-snapshot");
    let path = dir.join("image.img");
    let bytes = image();
    std::fs::write(&path, &bytes).unwrap();
    let before = snapshot_image(&path);
    assert_image_unchanged(&path, &before, "untouched");
    assert_file_bytes(&path, &bytes, "untouched");
    let copy = dir.join("copy.img");
    copy_sparse_at(&path, &copy, 0);
    assert_eq!(first_difference(&path, &copy), None);

    write_image_region(&path, (2 << 20) + 18, &[1]);
    assert_eq!(
        first_difference(&path, &copy).as_deref(),
        Some("first differing byte at 0x200012: 0x01 and 0x00")
    );
    assert_eq!(read_image_region(&path, (2 << 20) + 17, 2), [0xA5, 1]);
}

#[test]
#[should_panic(expected = "the image changed: lengths differ")]
fn a_longer_image_is_a_changed_image() {
    let dir = ScratchDir::new("bounded-length");
    let path = dir.join("image.img");
    std::fs::write(&path, b"short").unwrap();
    let before = snapshot_image(&path);
    std::fs::write(&path, b"longer").unwrap();
    assert_image_unchanged(&path, &before, "the image changed");
}

#[test]
fn an_embedded_copy_starts_at_its_offset() {
    let dir = ScratchDir::new("bounded-embed");
    let path = dir.join("image.img");
    let bytes = image();
    std::fs::write(&path, &bytes).unwrap();
    let embedded = dir.join("embedded.img");
    copy_sparse_at(&path, &embedded, 4096);
    let mut want = vec![0u8; 4096];
    want.extend_from_slice(&bytes);
    assert_file_bytes(&embedded, &want, "embedded");
    assert_eq!(sha256_file(&embedded), sha256_hex(&want));
}

#[test]
fn a_small_file_is_read_whole() {
    let dir = ScratchDir::new("bounded-small");
    let path = dir.join("small");
    let bytes = vec![3u8; SMALL_FILE as usize];
    std::fs::write(&path, &bytes).unwrap();
    assert!(read_small_file(&path) == bytes);
}

#[test]
#[should_panic(expected = "a test reads whole")]
fn a_larger_file_is_not() {
    let dir = ScratchDir::new("bounded-large");
    let path = dir.join("large");
    std::fs::write(&path, vec![0u8; SMALL_FILE as usize + 1]).unwrap();
    read_small_file(&path);
}
