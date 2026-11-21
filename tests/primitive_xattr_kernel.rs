//! `primitives::inspect_xattrs` against the Linux kernel (#311).
//!
//! The tree is staged in the guest, where `setfattr` means what it says,
//! and `mkfs.erofs` stores an attribute two files share in the shared
//! area and the rest inline. On the healthy image every path's `user.*`
//! attributes must be the ones the kernel lists. With one inline entry
//! destroyed, or one shared id pointed past the image, the evidence must
//! report the damage on that file and keep every attribute the kernel
//! still lists (the kernel refuses the first file's attributes, and leaves
//! the unreadable shared one out of the second's without a word), and
//! every attribute it keeps must be the file's own.

mod common;

use common::{reseal_superblock, stage_image, MemDev};
use fs_erofs::primitives::{
    inspect_inode, inspect_superblock, inspect_xattrs, Geometry, XattrEvidence,
};
use fs_erofs::xattr::resolve_full_name;
use fs_erofs_test_support::{guest_kernel_read, mkfs_from_guest_tree, ScratchDir};
use std::collections::BTreeMap;

const STAGE: &str = r#"
touch solo pair-a pair-b plain
python3 - <<'PY'
import os
for name, value in [(b'user.a', b'first'), (b'user.b', b'second'), (b'user.c', b'third')]:
    os.setxattr(b'solo', name, value)
for p in [b'pair-a', b'pair-b']:
    os.setxattr(p, b'user.shared', b'held once, used twice')
    os.setxattr(p, b'user.own-' + p, p)
PY
"#;

const PATHS: [&str; 4] = ["solo", "pair-a", "pair-b", "plain"];

/// `name -> value`, or `None` when the kernel refuses the attributes.
type Attrs = Option<BTreeMap<Vec<u8>, Vec<u8>>>;

/// Every path's inode number and `user.*` attributes, by the kernel.
fn kernel_attrs(image: &std::path::Path) -> Vec<(String, u64, Attrs)> {
    let script = format!(
        r#"python3 - "$MNT" <<'PY'
import os, sys
for rel in {PATHS:?}:
    p = os.path.join(sys.argv[1], rel)
    ino = os.stat(p).st_ino
    try:
        attrs = {{n: os.getxattr(p, n) for n in os.listxattr(p) if n.startswith('user.')}}
        print(rel, ino, ' '.join(n.encode().hex() + '=' + v.hex() for n, v in sorted(attrs.items())) or '-')
    except OSError:
        print(rel, ino, 'ERR')
PY
"#
    );
    let out = guest_kernel_read(image.to_str().expect("utf-8 image path"), &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let unhex = |s: &str| {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect::<Vec<u8>>()
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.splitn(3, ' ').collect();
            let attrs = match f[2] {
                "ERR" => None,
                "-" => Some(BTreeMap::new()),
                list => Some(
                    list.split(' ')
                        .map(|kv| {
                            let (k, v) = kv.split_once('=').unwrap();
                            (unhex(k), unhex(v))
                        })
                        .collect(),
                ),
            };
            (f[0].to_string(), f[1].parse().unwrap(), attrs)
        })
        .collect()
}

fn geometry(bytes: &[u8]) -> Geometry {
    inspect_superblock(&MemDev::new(bytes.to_vec()))
        .geometry()
        .expect("geometry")
}

fn evidence(bytes: &[u8], geometry: &Geometry, nid: u64) -> XattrEvidence {
    let source = MemDev::new(bytes.to_vec());
    let inode = inspect_inode(&source, geometry, nid).inode.expect("inode");
    inspect_xattrs(&source, geometry, &inode)
}

fn user_attrs(evidence: &XattrEvidence) -> BTreeMap<Vec<u8>, Vec<u8>> {
    evidence
        .entries
        .iter()
        .map(|e| (resolve_full_name(e.name_index, &e.name), e.value.clone()))
        .filter(|(n, _)| n.starts_with(b"user."))
        .collect()
}

#[test]
fn xattrs_read_as_the_kernel_lists_them_and_damage_costs_only_the_damaged_entry() {
    let dir = ScratchDir::new("xattr-primitive");
    let healthy_path = dir.join("healthy.img");
    let built = mkfs_from_guest_tree(&healthy_path, STAGE, &["-b4096", "-x1"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let healthy = std::fs::read(&healthy_path).unwrap();
    let geometry = geometry(&healthy);

    let kernel = kernel_attrs(&healthy_path);
    let mut truth = BTreeMap::new();
    for (path, nid, attrs) in &kernel {
        let attrs = attrs
            .clone()
            .unwrap_or_else(|| panic!("{path}: the kernel refuses a healthy image"));
        let ours = evidence(&healthy, &geometry, *nid);
        assert!(ours.findings.is_empty(), "{path}: {:?}", ours.findings);
        assert_eq!(user_attrs(&ours), attrs, "{path}");
        truth.insert(path.clone(), (*nid, attrs));
    }
    let solo = evidence(&healthy, &geometry, truth["solo"].0);
    assert!(solo.entries.iter().all(|e| e.shared_id.is_none()) && solo.entries.len() >= 3);
    let pair = evidence(&healthy, &geometry, truth["pair-a"].0);
    assert!(
        pair.entries.iter().any(|e| e.shared_id.is_some()),
        "fixture: pair-a's common attribute is shared"
    );
    let pair_inode = inspect_inode(&MemDev::new(healthy.clone()), &geometry, truth["pair-a"].0)
        .inode
        .unwrap();
    // The first shared id, after the 12-byte inline xattr header.
    let shared_id_at =
        (geometry.inode_offset(pair_inode.nid) + u64::from(pair_inode.on_disk_size) + 12) as usize;

    // Solo's second inline entry claims a 255-byte name; pair-a's shared
    // id points past the image.
    let second = solo.entries[1].offset as usize;
    let cases: [(&str, &str, usize, &[u8]); 2] = [
        ("an inline entry", "solo", second, &[255]),
        (
            "a shared id",
            "pair-a",
            shared_id_at,
            &[0xF0, 0xFF, 0xFF, 0x0F],
        ),
    ];
    let mut problems = Vec::new();
    for (what, hit, at, with) in cases {
        let mut bytes = healthy.clone();
        bytes[at..at + with.len()].copy_from_slice(with);
        // A small image keeps its metadata in the first block, which the
        // superblock checksum covers: resealed, so the kernel judges the
        // attribute and not the checksum.
        reseal_superblock(&mut bytes);
        let (image, _guard) = stage_image("xattr-damage", &bytes);
        for (path, nid, kernel) in kernel_attrs(&image) {
            let ours = evidence(&bytes, &geometry, nid);
            let kept = user_attrs(&ours);
            for (name, value) in &kept {
                if truth[&path].1.get(name) != Some(value) {
                    problems.push(format!("[{what}] {path}: {name:?} is not the file's own"));
                }
            }
            match kernel {
                // The kernel leaves out a shared attribute it cannot read
                // and says nothing; the evidence keeps the same ones and
                // reports the one left out.
                Some(kernel) if kept != kernel => problems.push(format!(
                    "[{what}] {path}: the kernel lists {kernel:?}, the evidence {kept:?}, {:?}",
                    ours.findings
                )),
                None if ours.findings.is_empty() => problems.push(format!(
                    "[{what}] {path}: the kernel refuses it, no finding"
                )),
                _ => {}
            }
            if path == hit && (kept.is_empty() || ours.findings.is_empty()) {
                problems.push(format!("[{what}] {path}: nothing kept, or no finding"));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
