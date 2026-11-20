//! `primitives::scan_inodes` against the Linux kernel (#311).
//!
//! Every inode the kernel reaches by walking the mounted tree must be a
//! candidate of a scan of the metadata area, with the kernel's mode and
//! size: with the superblock's geometry, and with the superblock
//! destroyed, from a hypothesis that the metadata area starts at block 0.
//! A scan stopped part way and resumed from its report finds the same
//! candidates as one that ran through.

mod common;

use common::{build_with_mkfs_erofs, dir, file, mixed_run_files, MemDev};
use fs_erofs::primitives::{inspect_superblock, scan_inodes, Geometry};
use fs_erofs_test_support::guest_kernel_read;
use std::collections::BTreeMap;
use std::ops::ControlFlow;

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let files = mixed_run_files();
    let names: Vec<String> = (0..120).map(|i| format!("n{i:03}")).collect();
    dir(vec![
        (
            "data",
            dir(files.iter().map(|(n, d)| (n.as_str(), file(d))).collect()),
        ),
        (
            "many",
            dir(names
                .iter()
                .map(|n| (n.as_str(), file(n.as_bytes())))
                .collect()),
        ),
        ("empty", dir(vec![])),
    ])
}

/// Every inode the kernel reaches: inode number -> (raw mode, size).
fn kernel_inodes(image: &std::path::Path) -> BTreeMap<u64, (u32, u64)> {
    let out = guest_kernel_read(
        image.to_str().expect("utf-8 image path"),
        r#"find "$MNT" -exec stat -c '%i %f %s' {} +"#,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            (
                f[0].parse().unwrap(),
                (
                    u32::from_str_radix(f[1], 16).unwrap(),
                    f[2].parse().unwrap(),
                ),
            )
        })
        .collect()
}

fn scan(bytes: &[u8], geometry: &Geometry) -> BTreeMap<u64, (u32, u64)> {
    let mut found = BTreeMap::new();
    let report = scan_inodes(&MemDev::new(bytes.to_vec()), geometry, 0..u64::MAX, |e| {
        let inode = e.inode.expect("a candidate decodes");
        found.insert(e.nid, (u32::from(inode.mode), inode.size));
        ControlFlow::Continue(())
    });
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.candidates, found.len() as u64);
    found
}

#[test]
fn every_inode_the_kernel_reaches_is_a_candidate_with_and_without_the_superblock() {
    for args in [&["-b4096"][..], &["-b4096", "-zlz4", "-Eztailpacking"][..]] {
        let img = build_with_mkfs_erofs(args, &tree());
        let kernel = kernel_inodes(&img.path);
        let evidence = inspect_superblock(&MemDev::new(img.bytes.clone()));
        let geometry = evidence.geometry().expect("geometry");

        let found = scan(&img.bytes, &geometry);
        for (nid, stat) in &kernel {
            assert_eq!(found.get(nid), Some(stat), "{args:?}: nid {nid}");
        }
        println!(
            "{args:?}: {} inodes, {} candidates",
            kernel.len(),
            found.len()
        );

        // The superblock destroyed: block 0 as the metadata area's start,
        // so each inode is found at its byte offset / 32.
        let mut bytes = img.bytes.clone();
        bytes[1024..1152].fill(0);
        assert!(inspect_superblock(&MemDev::new(bytes.clone()))
            .geometry()
            .is_none());
        let shift = u64::from(geometry.meta_blkaddr) * geometry.block_size / 32;
        let hypothesis =
            Geometry::hypothesis(4096, 0, 0, bytes.len() as u64).expect("4 KiB hypothesis");
        let found = scan(&bytes, &hypothesis);
        for (nid, stat) in &kernel {
            assert_eq!(
                found.get(&(nid + shift)),
                Some(stat),
                "{args:?}: nid {nid}, no superblock"
            );
        }
    }
}

#[test]
fn a_scan_stopped_and_resumed_finds_what_one_run_finds() {
    let img = build_with_mkfs_erofs(&["-b4096"], &tree());
    let geometry = inspect_superblock(&MemDev::new(img.bytes.clone()))
        .geometry()
        .unwrap();
    let whole: Vec<u64> = scan(&img.bytes, &geometry).into_keys().collect();
    let source = MemDev::new(img.bytes.clone());
    let mut resumed = Vec::new();
    let mut start = 0;
    loop {
        let mut taken = 0;
        let report = scan_inodes(&source, &geometry, start..u64::MAX, |e| {
            resumed.push(e.nid);
            taken += 1;
            if taken == 7 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        if taken < 7 {
            break;
        }
        assert!(report.next > start, "the scan made no progress");
        start = report.next;
    }
    assert_eq!(resumed, whole);
}
