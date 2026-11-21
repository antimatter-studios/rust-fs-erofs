//! `primitives::inspect_inode` against the Linux kernel (#311).
//!
//! The kernel numbers an EROFS inode by its NID, so `stat`'s `%i` names
//! the inode to inspect. On images `mkfs.erofs` built, compact and
//! extended, every path's inode must decode with no findings and with the
//! mode, owner, link count and size the kernel reports. On copies with
//! one inode damaged so the kernel refuses to load it, the evidence must
//! have a finding for that inode, and none for the inodes the kernel
//! still loads.

mod common;

use common::{build_with_mkfs_erofs, dir, file, stage_image, MemDev};
use fs_erofs::primitives::{inspect_inode, inspect_superblock, Geometry, InodeEvidence};
use fs_erofs_test_support::guest_kernel_read;
use std::collections::BTreeMap;

/// Enough inodes that some lie past the first block, which the
/// superblock checksum covers: damage there would be refused at mount,
/// for the checksum, before any inode is loaded.
const PAD: usize = 150;

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let big: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let pad: Vec<(String, Vec<u8>)> = (0..PAD)
        .map(|i| (format!("p{i:03}"), format!("pad {i}\n").into_bytes()))
        .collect();
    let late = vec![
        ("l0.txt", file(b"late 0\n")),
        ("l1.txt", file(b"late 1\n")),
        ("l2.txt", file(b"late 2\n")),
    ];
    dir(vec![
        (
            "pad",
            dir(pad.iter().map(|(n, d)| (n.as_str(), file(d))).collect()),
        ),
        ("zlate", dir(late)),
        ("small.txt", file(b"small\n")),
        ("big.bin", file(&big)),
        ("empty", file(b"")),
        (
            "sub",
            dir(vec![("c.txt", file(b"nested\n")), ("deeper", dir(vec![]))]),
        ),
    ])
}

/// What the kernel reports for one path: `None` when it cannot load it.
type KernelStat = Option<(u64, u32, u32, u32, u32, u64)>;

/// `stat` of every path of the healthy tree, by the mounted kernel:
/// inode number, raw mode, uid, gid, links and size. `None` when the
/// kernel cannot stat the path, or cannot read it (list a directory, read
/// a file): some damage, an inline tail past its block for one, is found
/// only when the data is read.
fn kernel_stats(image: &std::path::Path, paths: &[&str]) -> BTreeMap<String, KernelStat> {
    let mut script = String::from(
        "while IFS= read -r p; do\n    \
         if s=\"$(stat -c '%i %f %u %g %h %s' -- \"$MNT/$p\" 2>/dev/null)\" \
         && if [ -d \"$MNT/$p\" ]; then ls -- \"$MNT/$p\"; else cat -- \"$MNT/$p\"; fi \
         > /dev/null 2>&1; then\n        \
         printf '%s|%s\\n' \"$p\" \"$s\"\n    \
         else printf '%s|ERR\\n' \"$p\"; fi\n\
         done <<'PATHS'\n",
    );
    for path in paths {
        script.push_str(path);
        script.push('\n');
    }
    script.push_str("PATHS\n");
    let out = guest_kernel_read(image.to_str().expect("utf-8 image path"), &script);
    assert!(
        out.status.success(),
        "the kernel's stat walk failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let (path, rest) = line.rsplit_once('|').expect("path|stat");
            let stat = (rest != "ERR").then(|| {
                let f: Vec<&str> = rest.split(' ').collect();
                (
                    f[0].parse().unwrap(),
                    u32::from_str_radix(f[1], 16).unwrap(),
                    f[2].parse().unwrap(),
                    f[3].parse().unwrap(),
                    f[4].parse().unwrap(),
                    f[5].parse().unwrap(),
                )
            });
            (path.to_string(), stat)
        })
        .collect()
}

const PATHS: [&str; 12] = [
    ".",
    "small.txt",
    "big.bin",
    "empty",
    "sub",
    "sub/c.txt",
    "sub/deeper",
    "pad",
    "zlate",
    "zlate/l0.txt",
    "zlate/l1.txt",
    "zlate/l2.txt",
];

fn inspect(bytes: &[u8], geometry: &Geometry, nid: u64) -> InodeEvidence {
    inspect_inode(&MemDev::new(bytes.to_vec()), geometry, nid)
}

fn geometry(bytes: &[u8]) -> Geometry {
    inspect_superblock(&MemDev::new(bytes.to_vec()))
        .geometry()
        .expect("geometry")
}

/// Every inode of a healthy image decodes with no findings, as the
/// kernel reports it.
#[test]
fn every_inode_the_kernel_loads_decodes_as_the_kernel_reports_it() {
    for args in [&["-b4096"][..], &["-b4096", "-Eforce-inode-extended"][..]] {
        let img = build_with_mkfs_erofs(args, &tree());
        let geometry = geometry(&img.bytes);
        let stats = kernel_stats(&img.path, &PATHS);
        for (path, stat) in &stats {
            let (ino, mode, uid, gid, nlink, size) =
                stat.unwrap_or_else(|| panic!("{args:?} {path}: the kernel cannot stat it"));
            let evidence = inspect(&img.bytes, &geometry, ino);
            assert!(
                evidence.findings.is_empty(),
                "{args:?} {path} (nid {ino}): {:#?}",
                evidence.findings
            );
            let inode = evidence.inode.expect("decoded");
            assert_eq!(
                (
                    u32::from(inode.mode),
                    inode.uid,
                    inode.gid,
                    inode.nlink,
                    inode.size
                ),
                (mode, uid, gid, nlink, size),
                "{args:?} {path} (nid {ino}): mode, uid, gid, links, size"
            );
        }
    }
}

/// Damage the kernel refuses an inode for is a finding on that inode,
/// and the inodes it still loads have none.
#[test]
fn every_inode_the_kernel_refuses_has_a_finding() {
    let img = build_with_mkfs_erofs(&["-b4096"], &tree());
    let geometry = geometry(&img.bytes);
    let stats = kernel_stats(&img.path, &PATHS);
    let nid = |path: &str| stats[path].expect("healthy").0;
    // A small file whose inode lies past the checksummed first block.
    let target_path = ["zlate/l0.txt", "zlate/l1.txt", "zlate/l2.txt"]
        .into_iter()
        .find(|path| geometry.inode_offset(nid(path)) >= geometry.block_size)
        .expect("fixture: some late inode lies past the first block");
    let target = nid(target_path);
    let at = geometry.inode_offset(target) as usize;

    type Damage = (&'static str, fn(&mut [u8]));
    let damages: [Damage; 3] = [
        ("data layout 7", |inode| {
            inode[0] = (inode[0] & 1) | (7 << 1)
        }),
        ("a mode with no file type", |inode| {
            inode[4..6].copy_from_slice(&0o644u16.to_le_bytes())
        }),
        ("an inline tail past its block", |inode| {
            inode[8..12].copy_from_slice(&4095u32.to_le_bytes())
        }),
    ];
    let mut problems = Vec::new();
    for (what, damage) in damages {
        let mut bytes = img.bytes.clone();
        damage(&mut bytes[at..at + 64]);
        let (image, _guard) = stage_image("inode-damage", &bytes);
        let kernel = kernel_stats(&image, &PATHS);
        if kernel[target_path].is_some() {
            problems.push(format!("[{what}] the kernel still loads the damaged inode"));
        }
        for path in PATHS {
            let Some(stat) = kernel[path] else { continue };
            let evidence = inspect(&bytes, &geometry, stat.0);
            if !evidence.findings.is_empty() {
                problems.push(format!(
                    "[{what}] {path}: the kernel loads it, and the evidence finds {:?}",
                    evidence.findings
                ));
            }
        }
        let evidence = inspect(&bytes, &geometry, target);
        if evidence.findings.is_empty() {
            problems.push(format!(
                "[{what}] the kernel refuses {target_path} and there is no finding"
            ));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
