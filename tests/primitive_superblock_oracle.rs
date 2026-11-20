//! `primitives::inspect_superblock` against erofs-utils (#311).
//!
//! On images `mkfs.erofs` built, the geometry the evidence states must be
//! the one `dump.erofs -s` prints, and the evidence must find nothing
//! wrong. On copies with one superblock field destroyed, an independent
//! reader must refuse each copy -- `fsck.erofs`, or where it does not
//! look (it accepts a superblock whose checksum does not match), the
//! kernel's mount -- and the evidence must name the field at its offset.

mod common;

use common::{build_with_mkfs_erofs, dir, file, stage_image, MemDev};
use fs_erofs::primitives::{inspect_superblock, SuperblockEvidence};
use fs_erofs_test_support::{guest_kernel_read_if_mounted, oracle};
use std::collections::BTreeMap;

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    dir(vec![
        ("a.txt", file(b"hello\n")),
        ("big.bin", file(&big)),
        ("sub", dir(vec![("c.txt", file(b"nested\n"))])),
    ])
}

/// `dump.erofs -s`, as `key -> value` with the `Filesystem ` prefix and
/// the padding dropped.
fn dump_superblock(path: &std::path::Path) -> BTreeMap<String, String> {
    let out = oracle("dump.erofs").arg("-s").arg(path).output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "dump.erofs -s {}:\n{stdout}{}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| {
            let key = key.trim().trim_start_matches("Filesystem ").to_string();
            (key, value.trim().to_string())
        })
        .collect()
}

fn dumped(dump: &BTreeMap<String, String>, key: &str) -> u64 {
    let value = dump
        .get(key)
        .unwrap_or_else(|| panic!("dump.erofs -s printed no `{key}`: {dump:#?}"));
    value
        .parse()
        .unwrap_or_else(|_| panic!("dump.erofs -s `{key}` is {value:?}, not a number"))
}

fn evidence(bytes: &[u8]) -> SuperblockEvidence {
    inspect_superblock(&MemDev::new(bytes.to_vec()))
}

/// Every geometry field the evidence states is the one `dump.erofs`
/// prints, at three shapes of image, and a healthy superblock has no
/// findings.
#[test]
fn the_superblock_evidence_states_the_geometry_dump_erofs_prints() {
    for args in [
        &["-b4096"][..],
        &["-b512"][..],
        &["-b4096", "-zlz4", "-Efragments,ztailpacking"][..],
    ] {
        let img = build_with_mkfs_erofs(args, &tree());
        let dump = dump_superblock(&img.path);
        let evidence = evidence(&img.bytes);
        assert!(
            evidence.findings.is_empty(),
            "{args:?}: findings on a healthy image: {:#?}",
            evidence.findings
        );
        let geometry = evidence
            .geometry()
            .expect("a healthy image states a geometry");
        assert_eq!(
            geometry.block_size,
            dumped(&dump, "blocksize"),
            "{args:?} block size"
        );
        assert_eq!(
            u64::from(geometry.meta_blkaddr),
            dumped(&dump, "inode metadata start block"),
            "{args:?} metadata start"
        );
        assert_eq!(
            u64::from(geometry.xattr_blkaddr),
            dumped(&dump, "shared xattr metadata start block"),
            "{args:?} shared xattr start"
        );
        assert_eq!(
            geometry.root_nid,
            dumped(&dump, "root nid"),
            "{args:?} root nid"
        );
        let fields = evidence.fields.as_ref().expect("fields");
        assert_eq!(
            u64::from(fields.blocks),
            dumped(&dump, "blocks"),
            "{args:?} blocks"
        );
        assert_eq!(
            fields.inos,
            dumped(&dump, "inode count"),
            "{args:?} inode count"
        );
        assert_eq!(evidence.checksum_matches, Some(true), "{args:?} checksum");
    }
}

/// What is destroyed, the byte the finding must name, and how.
type Damage = (&'static str, u64, fn(&mut Vec<u8>));

/// One superblock field destroyed per copy: `fsck.erofs` or the kernel
/// refuses every copy, and the evidence names the field, at its offset.
#[test]
fn every_superblock_damage_an_independent_reader_refuses_is_a_finding() {
    let healthy = build_with_mkfs_erofs(&["-b4096"], &tree()).bytes;
    let damages: [Damage; 4] = [
        ("magic", 1024, |b| b[1024..1028].fill(0)),
        ("block size", 1024 + 0x0C, |b| b[1024 + 0x0C] = 30),
        // A byte of the volume name: read by nothing but the checksum.
        ("checksum", 1024, |b| b[1024 + 0x40] ^= 0xFF),
        ("truncated", 1024 + 0x24, |b| b.truncate(b.len() / 2)),
    ];
    let mut problems = Vec::new();
    for (what, at, damage) in damages {
        let mut bytes = healthy.clone();
        damage(&mut bytes);
        let (path, _guard) = stage_image("superblock-damage", &bytes);
        let fsck = oracle("fsck.erofs").arg(&path).output();
        if fsck.status.code() == Some(0) {
            let image = path.to_str().expect("utf-8 image path");
            if guest_kernel_read_if_mounted(image, "true").is_ok() {
                problems.push(format!(
                    "[{what}] fsck.erofs accepts the damaged copy and the kernel mounts it"
                ));
            }
        }
        let evidence = evidence(&bytes);
        if !evidence.findings.iter().any(|f| f.offset == at) {
            problems.push(format!(
                "[{what}] no finding at byte {at}: {:?}",
                evidence.findings
            ));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
