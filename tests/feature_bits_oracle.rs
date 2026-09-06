//! Which `feature_incompat` bit each `mkfs.erofs` option actually sets.
//!
//! # Why this is measured rather than read off a header
//!
//! `EROFS_FEATURE_INCOMPAT_FRAGMENTS` was `0x10` in this crate for a
//! long time. `0x10` is ztailpacking's bit; fragments is `0x20`. No
//! image was ever misread, because nothing gated on the constant — and
//! that is exactly why it survived. A constant that is wrong and unused
//! reads as verified, and the next person to write a gate on fragments
//! would have gated on ztailpacking instead.
//!
//! So the values are checked against the tool that writes them:
//! `mkfs.erofs` builds an image with one option, and the bit it set is
//! read straight out of the superblock at offset 0x50.
//!
//! Skips when `mkfs.erofs` is not on `PATH`.

mod common;
use common::{materialize_tree, mkfs_erofs_available, run_mkfs_erofs};

use fs_erofs::mkfs;
use fs_erofs::superblock::{
    EROFS_FEATURE_INCOMPAT_DEDUPE, EROFS_FEATURE_INCOMPAT_FRAGMENTS,
    EROFS_FEATURE_INCOMPAT_ZERO_PADDING, EROFS_FEATURE_INCOMPAT_ZTAILPACKING,
};
use std::collections::BTreeMap;
use std::path::Path;

/// `feature_incompat` sits at 0x50 within the superblock, which begins
/// 1024 bytes into the image.
const FEATURE_INCOMPAT_AT: usize = 1024 + 0x50;

fn source_tree() -> mkfs::Node {
    // Big enough and varied enough that the encoder has something to
    // fragment and something to inline: a tree of tiny tails alongside
    // one file well over a block.
    let mut entries: BTreeMap<String, mkfs::Node> = BTreeMap::new();
    for i in 0..8 {
        entries.insert(
            format!("small{i}.txt"),
            mkfs::Node::File {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: format!("tail bytes for {i}\n").into_bytes(),
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        );
    }
    let mut big = Vec::with_capacity(200_000);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..200_000 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        big.push((x >> 16) as u8);
    }
    entries.insert(
        "big.bin".to_string(),
        mkfs::Node::File {
            mode: mkfs::DEFAULT_FILE_MODE,
            data: big,
            meta: mkfs::NodeMeta::default(),
            xattrs: Vec::new(),
        },
    );
    mkfs::Node::Dir {
        mode: mkfs::DEFAULT_DIR_MODE,
        entries,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

/// Build with `extra` and return the image's `feature_incompat`.
fn incompat_for(dir: &Path, label: &str, extra: &[&str]) -> u32 {
    let src = dir.join("src");
    let img = dir.join(format!("{label}.img"));
    let run = run_mkfs_erofs(extra, &img, &src);
    assert_eq!(
        run.status_code,
        Some(0),
        "mkfs.erofs {extra:?} failed: {}",
        run.stderr
    );
    let bytes = std::fs::read(&img).expect("read the built image");
    let raw: [u8; 4] = bytes[FEATURE_INCOMPAT_AT..FEATURE_INCOMPAT_AT + 4]
        .try_into()
        .expect("four bytes at 0x50");
    let bits = u32::from_le_bytes(raw);
    eprintln!("{label:<28} feature_incompat = {bits:#010x}");
    bits
}

#[test]
fn each_option_sets_the_bit_this_crate_names() {
    if !mkfs_erofs_available() {
        eprintln!("mkfs.erofs not on PATH — skipping");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    materialize_tree(&dir.path().join("src"), &source_tree());

    let plain = incompat_for(dir.path(), "plain", &["-zlz4hc"]);
    let fragments = incompat_for(dir.path(), "fragments", &["-zlz4hc", "-Efragments"]);
    let dedupe = incompat_for(dir.path(), "dedupe", &["-zlz4hc", "-Ededupe"]);
    let all = incompat_for(
        dir.path(),
        "fragments,dedupe,ztail",
        &["-zlz4hc", "-Efragments,dedupe,ztailpacking"],
    );

    // The baseline every compressed image carries.
    assert_eq!(
        plain, EROFS_FEATURE_INCOMPAT_ZERO_PADDING,
        "a plain compressed image should set ZERO_PADDING and nothing else"
    );

    // FRAGMENTS is what `-Efragments` adds on top of the baseline.
    assert_eq!(
        fragments ^ plain,
        EROFS_FEATURE_INCOMPAT_FRAGMENTS,
        "the bit -Efragments adds is EROFS_FEATURE_INCOMPAT_FRAGMENTS"
    );

    // DEDUPE IS THE SAME BIT, and on its own it sets nothing: the tool
    // only records it when fragments are in play. A reader therefore
    // cannot tell a deduplicated image from a plain one, and does not
    // need to.
    assert_eq!(
        EROFS_FEATURE_INCOMPAT_DEDUPE, EROFS_FEATURE_INCOMPAT_FRAGMENTS,
        "the kernel header aliases these onto one bit"
    );
    assert_eq!(
        dedupe, plain,
        "-Ededupe alone sets no bit of its own, so a deduplicated image \
         is indistinguishable from a plain one"
    );

    // ZTAILPACKING is the remaining bit once fragments is accounted for.
    assert_eq!(
        all ^ fragments,
        EROFS_FEATURE_INCOMPAT_ZTAILPACKING,
        "the bit ztailpacking adds is EROFS_FEATURE_INCOMPAT_ZTAILPACKING"
    );

    // And the two are not the same bit, which is the mistake this test
    // exists to stop coming back.
    assert_ne!(
        EROFS_FEATURE_INCOMPAT_FRAGMENTS, EROFS_FEATURE_INCOMPAT_ZTAILPACKING,
        "fragments is 0x20 and ztailpacking is 0x10; they were once both 0x10 here"
    );
}
