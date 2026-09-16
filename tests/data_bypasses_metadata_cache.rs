//! Reading file data does not displace the metadata cache (#69).
//!
//! `open_with_cache` wrapped the whole device in `CachingDevice`, and file
//! data was read through that same handle one block at a time. A one-block
//! read never trips the cache's large-read bypass (`spanned > 1`), so every
//! data block was cached on its merits: a 2 MiB file at 4 KiB blocks is 512
//! reads, exactly the default 512-block capacity, and one `read_file`
//! evicted every metadata block a directory walk had warmed.
//!
//! The counter sits below the cache, so it reports what reached the device.

mod common;
use common::{dir, file};

use fs_core::{CountingDevice, FileDevice};
use fs_erofs::fs::DEFAULT_METADATA_CACHE_BLOCKS;
use fs_erofs::{mkfs, Filesystem};
use std::sync::Arc;

const BIG: usize = 2 * 1024 * 1024;

fn fixture() -> Vec<u8> {
    let names: Vec<String> = (0..8).map(|d| format!("d{d}")).collect();
    let leaves: Vec<Vec<String>> = (0..8)
        .map(|d| (0..20).map(|f| format!("f{d}_{f}.txt")).collect())
        .collect();
    let mut top: Vec<(&str, mkfs::Node)> = names
        .iter()
        .zip(leaves.iter())
        .map(|(d, fs)| {
            (
                d.as_str(),
                dir(fs
                    .iter()
                    .map(|f| (f.as_str(), file(f.as_bytes())))
                    .collect()),
            )
        })
        .collect();
    // Varied bytes, so nothing about the data is compressible or sparse.
    let big: Vec<u8> = (0..BIG).map(|i| (i * 7 + i / 4093) as u8).collect();
    top.push(("big.bin", file(&big)));
    mkfs::build_image(dir(top), 12).expect("build the fixture")
}

fn walk(fs: &Filesystem) -> usize {
    let mut seen = 0;
    for d in 0..8 {
        for f in 0..20 {
            fs.lookup_path(&format!("/d{d}/f{d}_{f}.txt"))
                .expect("lookup");
            seen += 1;
        }
    }
    seen
}

#[test]
fn reading_a_large_file_leaves_warm_metadata_warm() {
    let tmp = tempfile::NamedTempFile::new().expect("temp image");
    std::fs::write(tmp.path(), fixture()).expect("write the fixture");
    let counting = Arc::new(CountingDevice::new(Arc::new(
        FileDevice::open(tmp.path()).expect("open"),
    )));
    let fs = Filesystem::open_with_cache(counting.clone(), DEFAULT_METADATA_CACHE_BLOCKS)
        .expect("open with the metadata cache");

    assert_eq!(walk(&fs), 160);
    counting.reset();
    walk(&fs);
    assert_eq!(
        counting.reads(),
        0,
        "control: a second walk is served entirely from the metadata cache"
    );

    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    let mut buf = vec![0u8; inode.size as usize];
    counting.reset();
    fs.read_file(&inode, 0, &mut buf).expect("read big.bin");
    assert!(
        counting.reads() > 0,
        "control: the data read reached the device"
    );
    assert!(buf
        .iter()
        .enumerate()
        .all(|(i, &b)| b == (i * 7 + i / 4093) as u8));

    counting.reset();
    walk(&fs);
    assert_eq!(
        counting.reads(),
        0,
        "after reading a {BIG}-byte file the same walk went back to the device {} times: \
         file data displaced the metadata cache",
        counting.reads()
    );
}
