//! rust-fs-core's conformance helpers over this driver's layers (#311).
//!
//! `check_healthy` and `check_damaged` (fs_core::layers::conformance) hold
//! `ErofsVolume` to the contract of each layer, and the salvage set
//! (`conformance::salvage::run`) holds `ErofsSalvage` to the salvage
//! verbs' five rules, over images this crate's builder writes with one
//! structure damaged per copy. Conformance proves the shape of the
//! contract; whether the layers read EROFS correctly is the kernel's call,
//! in tests/layers_kernel.rs.

mod common;

use common::{dir, file, MemDev};
use fs_core::layers::conformance::salvage::{self, Damaged, Driver, Evidence};
use fs_core::layers::conformance::{check_damaged, check_healthy, Expected, Limits};
use fs_core::layers::{DamageKind, Extent, OpenVolume};
use fs_core::BlockRead;
use fs_erofs::layers::{ErofsSalvage, ErofsVolume};
use fs_erofs::primitives::inspect_superblock;
use fs_erofs::{mkfs, Filesystem, Inode};
use std::sync::Arc;

const BLOCK: usize = 4096;

fn wide_names() -> Vec<String> {
    (0..300).map(|i| format!("entry-{i:03}")).collect()
}

fn big() -> Vec<u8> {
    (0..5 * BLOCK as u32 + 123)
        .map(|i| (i % 251) as u8)
        .collect()
}

fn tree() -> mkfs::Node<'static> {
    let names = wide_names();
    dir(vec![
        ("hello.txt", file(b"hello\n")),
        ("big.bin", file(&big())),
        ("empty", file(b"")),
        (
            "wide",
            dir(names
                .iter()
                .map(|n| (n.as_str(), file(n.as_bytes())))
                .collect()),
        ),
        ("docs", dir(vec![("readme", file(b"read me\n"))])),
    ])
}

fn healthy() -> Vec<u8> {
    mkfs::build_image(tree(), 12).expect("build_image")
}

fn source(bytes: &[u8]) -> Arc<dyn BlockRead> {
    MemDev::arc(bytes.to_vec())
}

fn inode_of(bytes: &[u8], path: &str) -> Inode {
    let fs = Filesystem::open(source(bytes)).expect("open");
    fs.lookup_path(path).expect("lookup")
}

/// The damaged copies: each loses one structure, and each must leave its
/// evidence on the salvage path while the strict reader refuses it.
fn damaged() -> Vec<Damaged> {
    let healthy = healthy();
    let geometry = inspect_superblock(&MemDev::new(healthy.clone()))
        .geometry()
        .expect("geometry");
    let mut out = Vec::new();

    let mut magic = healthy.clone();
    magic[1024..1028].fill(0);
    out.push(Damaged::new(
        "superblock magic destroyed",
        magic,
        Evidence::Damage(DamageKind::Signature),
    ));

    let mut checksum = healthy.clone();
    checksum[1024 + 0x40] ^= 0xFF;
    out.push(Damaged::new(
        "superblock checksum mismatch",
        checksum,
        Evidence::Damage(DamageKind::Checksum),
    ));

    let wide = inode_of(&healthy, "/wide");
    let mut directory = healthy.clone();
    let at = wide.raw_u as usize * BLOCK;
    directory[at..at + BLOCK].fill(0xFF);
    out.push(Damaged::new(
        "a directory block destroyed",
        directory,
        Evidence::Damage(DamageKind::Malformed),
    ));

    let big = inode_of(&healthy, "/big.bin");
    let mut moved = healthy.clone();
    let at = geometry.inode_offset(big.nid) as usize;
    moved[at + 0x10..at + 0x14].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    out.push(Damaged::new(
        "a file's blocks moved past the source",
        moved,
        Evidence::BrokenWalk,
    ));

    let mut truncated = healthy.clone();
    truncated.truncate(healthy.len() - 2 * BLOCK);
    out.push(Damaged::new(
        "the image's last blocks cut off",
        truncated,
        Evidence::Damage(DamageKind::Missing),
    ));

    let data = big.raw_u as u64 * BLOCK as u64;
    out.push(
        Damaged::new(
            "a sector of file data unreadable",
            healthy.clone(),
            Evidence::Damage(DamageKind::Unreadable),
        )
        .unreadable(Extent::new(data + 512, 512)),
    );
    out
}

struct Erofs;

impl Driver for Erofs {
    type Adapter = ErofsSalvage;
    type StrictError = String;

    fn salvage(&self, source: &dyn BlockRead) -> ErofsSalvage {
        ErofsSalvage::new(source).unwrap_or_else(|_| {
            // No superblock: the hypothesis every image this builder writes
            // satisfies, metadata from block 0.
            ErofsSalvage::from_hypothesis(
                fs_erofs::primitives::Geometry::hypothesis(BLOCK as u64, 0, 0, source.size_bytes())
                    .expect("4 KiB"),
            )
        })
    }

    fn strict(&self, source: &dyn BlockRead) -> Result<(), String> {
        let mut bytes = vec![0u8; source.size_bytes() as usize];
        source.read_at(0, &mut bytes).map_err(|e| e.to_string())?;
        let fs = Filesystem::open(MemDev::arc(bytes)).map_err(|e| e.to_string())?;
        let mut queue = vec![fs.root_inode().map_err(|e| e.to_string())?];
        while let Some(dir) = queue.pop() {
            for entry in fs.read_dir(&dir).map_err(|e| e.to_string())? {
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                let inode = fs.read_inode(entry.nid).map_err(|e| e.to_string())?;
                if inode.is_dir() {
                    queue.push(inode);
                } else {
                    let mut buf = vec![0u8; inode.size as usize];
                    fs.read_file(&inode, 0, &mut buf)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    fn healthy(&self) -> Vec<u8> {
        healthy()
    }

    fn damaged(&self) -> Vec<Damaged> {
        damaged()
    }
}

#[test]
fn the_salvage_adapter_conforms() {
    salvage::run(&Erofs).assert_conforms();
}

#[test]
fn a_healthy_volume_passes_every_layer_check() {
    let volume = ErofsVolume::open(source(&healthy())).expect("open");
    let mut expected = vec![
        Expected::file("/hello.txt", b"hello\n"),
        Expected::file("/big.bin", &big()),
        Expected::file("/empty", b""),
        Expected::file("/docs/readme", b"read me\n"),
        Expected::directory("/wide"),
        Expected::directory("/docs"),
        Expected::absent("/missing"),
    ];
    for name in wide_names().iter().step_by(37) {
        expected.push(Expected::file(&format!("/wide/{name}"), name.as_bytes()));
    }
    check_healthy(&volume, &expected, &Limits::default()).assert_passed();
}

#[test]
fn every_damaged_volume_opened_for_salvage_passes_the_volume_and_primitive_checks() {
    for case in damaged() {
        if !case.unreadable.is_empty() {
            continue; // MemDev has no unreadable ranges; the salvage set covers them.
        }
        let volume = ErofsVolume::open_for_salvage(source(&case.image)).expect("salvage open");
        let report = check_damaged(&volume, &Limits::default());
        assert!(report.passed(), "[{}] {report}", case.name);
    }
}
