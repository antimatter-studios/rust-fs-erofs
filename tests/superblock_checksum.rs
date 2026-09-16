//! The superblock checksum is verified at open (#52).
//!
//! `Superblock::verify_checksum` existed and only the writer's tests
//! called it: `Filesystem::open` read 128 bytes and trusted them, so a
//! superblock damaged in transit was accepted and the first symptom was
//! a confusing error much further in, or none. erofs-utils sets
//! `EROFS_FEATURE_COMPAT_SB_CHKSUM` on every image it writes (measured
//! on #52 across `-b 512/1024/4096/16384`), so this reader was the only
//! party in the chain not looking.
//!
//! The positive cases are images written by the real `mkfs.erofs`: the
//! committed `test-disks/erofs-zstd-4k.img` everywhere, and freshly built
//! images at four block sizes where the tool is installed. Those are what
//! establish that the CRC range and algorithm agree with the external
//! implementation, rather than with this crate's own writer.

mod common;
use common::{build_with_mkfs_erofs, dir, file, mkfs_erofs_available, run_mkfs_erofs, MemDev};

use fs_core::BlockRead;
use fs_erofs::{mkfs, Error, Filesystem};
use std::path::PathBuf;
use std::sync::Arc;

const SB: usize = 1024;
const FEATURE_COMPAT_AT: usize = SB + 0x08;
/// `uuid`, parsed and otherwise unused by the reader: damage here changes
/// nothing but the checksum, so a refusal can only come from verifying it.
const UUID_AT: usize = SB + 0x30;

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-disks/erofs-zstd-4k.img");
    // Committed, so a missing file is a failure rather than a skip.
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn has_checksum(img: &[u8]) -> bool {
    u32::from_le_bytes(
        img[FEATURE_COMPAT_AT..FEATURE_COMPAT_AT + 4]
            .try_into()
            .unwrap(),
    ) & 1
        != 0
}

fn open_bytes(img: Vec<u8>) -> Result<Filesystem, Error> {
    Filesystem::open(Arc::new(MemDev::new(img)) as Arc<dyn BlockRead>)
}

fn assert_refused_for_checksum(img: Vec<u8>, what: &str) {
    match open_bytes(img) {
        Err(Error::BadSuperblock(why)) => assert!(
            why.contains("checksum"),
            "{what}: refused, but for {why:?} rather than the checksum"
        ),
        Err(other) => panic!("{what}: refused for the wrong reason: {other:?}"),
        Ok(_) => panic!("{what}: the superblock fails its own CRC32C and the image opened anyway"),
    }
}

#[test]
fn a_real_mkfs_erofs_image_verifies_and_opens() {
    let img = fixture();
    assert!(
        has_checksum(&img),
        "the fixture no longer carries a checksum, so this test verifies nothing"
    );
    let fs = open_bytes(img).expect("the committed mkfs.erofs image must verify");
    assert!(fs.lookup_path("/hello.txt").is_ok());
}

#[test]
fn a_damaged_superblock_is_refused() {
    let mut img = fixture();
    img[UUID_AT] ^= 0x01;
    assert_refused_for_checksum(img, "one uuid bit flipped");
}

/// The range runs to the end of the superblock's block, not just the 128
/// bytes that are parsed.
#[test]
fn damage_past_the_parsed_128_bytes_is_refused() {
    let tree = dir(vec![("a.txt", file(b"alpha\n"))]);
    let mut img = mkfs::build_image(tree, 12).expect("build");
    assert!(has_checksum(&img));
    // Last byte of the checksummed range for a 4 KiB block.
    img[4095] ^= 0x80;
    assert_refused_for_checksum(img, "last byte of block 0 flipped");
}

/// Without the compat bit there is no checksum to verify, and an image
/// that does not advertise one still opens.
#[test]
fn an_image_without_the_checksum_bit_is_not_verified() {
    let mut img = fixture();
    let compat = u32::from_le_bytes(
        img[FEATURE_COMPAT_AT..FEATURE_COMPAT_AT + 4]
            .try_into()
            .unwrap(),
    );
    img[FEATURE_COMPAT_AT..FEATURE_COMPAT_AT + 4].copy_from_slice(&(compat & !1).to_le_bytes());
    img[UUID_AT] ^= 0x01;
    open_bytes(img).expect("no checksum advertised, nothing to verify");
}

/// Block sizes at and below the superblock offset use a different span
/// (`block_size - 1024 % block_size`); the writer's own images at every
/// size must still open.
#[test]
fn every_block_size_this_writer_emits_verifies() {
    for blkszbits in [9u8, 10, 12, 14] {
        let tree = dir(vec![("a.txt", file(b"alpha\n"))]);
        let img = mkfs::build_image(tree, blkszbits).expect("build");
        assert!(has_checksum(&img), "blkszbits {blkszbits}");
        open_bytes(img).unwrap_or_else(|e| panic!("blkszbits {blkszbits}: {e}"));
    }
}

/// The external implementation at every block size it will write.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_every_block_size_verifies_and_damage_is_refused() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let tree = dir(vec![
        ("a.txt", file(b"alpha\n")),
        ("sub", dir(vec![("b.txt", file(&[7u8; 9000]))])),
    ]);
    let mut verified = Vec::new();
    for bs in ["512", "1024", "4096", "16384"] {
        let flag = format!("-b{bs}");
        // mkfs.erofs refuses a block size above the host's page size --
        // 16 KiB on a 4 KiB Linux runner, the Apple silicon page size it
        // is here for. That refusal is the tool's, not a verdict on the
        // reader, so that size is skipped and said so; the smaller three
        // must still run.
        let probe = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(probe.path().join("src")).unwrap();
        let tried = run_mkfs_erofs(
            &[&flag],
            &probe.path().join("probe.img"),
            &probe.path().join("src"),
        );
        if tried.status_code != Some(0) && tried.stderr.contains("invalid block size") {
            eprintln!(
                "skipping {flag}: this host's mkfs.erofs refuses it ({})",
                tried.stderr.trim()
            );
            continue;
        }
        verified.push(bs);
        let img = build_with_mkfs_erofs(&[&flag], &tree).bytes;
        assert!(has_checksum(&img), "mkfs.erofs {flag} wrote no checksum");
        let fs = open_bytes(img.clone())
            .unwrap_or_else(|e| panic!("mkfs.erofs {flag} image must verify: {e}"));
        assert!(fs.lookup_path("/sub/b.txt").is_ok(), "{flag}");

        let mut damaged = img;
        damaged[UUID_AT] ^= 0x01;
        assert_refused_for_checksum(damaged, &format!("mkfs.erofs {flag}, uuid bit flipped"));
    }
    for required in ["512", "1024", "4096"] {
        assert!(
            verified.contains(&required),
            "block size {required} was skipped, so the oracle checked less than it claims: {verified:?}"
        );
    }
}

/// The superblock `open` returns is the one whose checksum it verified.
///
/// The checksum span is a second read. A device that answers the first
/// read with altered fields and the second with the original bytes had
/// the altered superblock parsed and the original one checksummed, so the
/// altered fields were accepted. Found by Greptile on #103.
#[test]
fn the_superblock_returned_is_the_one_that_was_verified() {
    struct Shifty {
        image: Vec<u8>,
        first: std::sync::atomic::AtomicBool,
    }
    impl BlockRead for Shifty {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let o = offset as usize;
            buf.copy_from_slice(&self.image[o..o + buf.len()]);
            if offset == 1024 && self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                // The same bytes the checksum covers, one field altered.
                buf[UUID_AT - 1024] ^= 0x01;
            }
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.image.len() as u64
        }
    }
    let tree = dir(vec![("a.txt", file(b"alpha\n"))]);
    let image = mkfs::build_image(tree, 12).expect("build");
    assert!(
        has_checksum(&image),
        "fixture: the writer stamps a checksum"
    );
    let original = image[UUID_AT];
    let dev = Shifty {
        image,
        first: std::sync::atomic::AtomicBool::new(true),
    };
    let sb = fs_erofs::superblock::read(&dev).expect("the second read is the valid image");
    assert_eq!(
        sb.uuid[0], original,
        "the superblock returned carries the altered first read, not the verified bytes"
    );
}
