//! `primitives::inspect_directory` against the Linux kernel (#311).
//!
//! On an image `mkfs.erofs` built, every directory's entries, block by
//! block, must be the names and inode numbers the kernel lists, with no
//! findings. With one block of a three-block directory destroyed, that
//! block must be a finding, and the entries kept from the other blocks
//! must be exactly the names the kernel still finds, with the inodes it
//! finds them at.

mod common;

use common::{build_with_mkfs_erofs, dir, file, stage_image, MemDev};
use fs_erofs::primitives::{
    inspect_directory, inspect_inode, inspect_superblock, DirBlockEvidence, Geometry,
};
use fs_erofs_test_support::guest_kernel_read;
use std::collections::BTreeSet;

const WIDE: usize = 400;

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let names: Vec<String> = (0..WIDE).map(|i| format!("entry-{i:03}")).collect();
    dir(vec![
        ("top.txt", file(b"top\n")),
        (
            "wide",
            dir(names
                .iter()
                .map(|n| (n.as_str(), file(n.as_bytes())))
                .collect()),
        ),
        ("small", dir(vec![("a", file(b"a")), ("b", dir(vec![]))])),
    ])
}

/// `(inode number, name)` pairs, without `.` and `..`.
type Listing = BTreeSet<(u64, String)>;

/// What the kernel says each line of `script` (run in `$MNT`) lists.
fn kernel_lines(image: &std::path::Path, script: &str) -> Vec<String> {
    let out = guest_kernel_read(image.to_str().expect("utf-8 image path"), script);
    assert!(
        out.status.success(),
        "the kernel's walk failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// The kernel's listing of `dir` (relative to the root), by `ls -ai`.
fn kernel_listing(image: &std::path::Path, dir: &str) -> Listing {
    kernel_lines(image, &format!("cd \"$MNT\"/{dir} && ls -1ai"))
        .into_iter()
        .filter_map(|line| {
            let (ino, name) = line.trim_start().split_once(' ')?;
            (name != "." && name != "..").then(|| (ino.parse().unwrap(), name.to_string()))
        })
        .collect()
}

fn listing(blocks: &[DirBlockEvidence]) -> Listing {
    blocks
        .iter()
        .flat_map(|b| &b.entries)
        .filter(|e| e.name != b"." && e.name != b"..")
        .map(|e| (e.nid, String::from_utf8(e.name.clone()).unwrap()))
        .collect()
}

fn geometry(bytes: &[u8]) -> Geometry {
    inspect_superblock(&MemDev::new(bytes.to_vec()))
        .geometry()
        .expect("geometry")
}

fn directory(bytes: &[u8], geometry: &Geometry, nid: u64) -> Vec<DirBlockEvidence> {
    let source = MemDev::new(bytes.to_vec());
    let inode = inspect_inode(&source, geometry, nid)
        .inode
        .expect("the directory's inode decodes");
    inspect_directory(&source, geometry, &inode)
}

/// The inode number the kernel gives `path`.
fn kernel_nid(image: &std::path::Path, path: &str) -> u64 {
    kernel_lines(image, &format!("stat -c %i \"$MNT\"/{path}"))[0]
        .parse()
        .unwrap()
}

#[test]
fn every_directory_lists_its_entries_as_the_kernel_lists_them() {
    let img = build_with_mkfs_erofs(&["-b4096"], &tree());
    let geometry = geometry(&img.bytes);
    for path in [".", "wide", "small", "small/b"] {
        let nid = kernel_nid(&img.path, path);
        let blocks = directory(&img.bytes, &geometry, nid);
        assert!(
            blocks.iter().all(|b| b.findings.is_empty()),
            "{path}: {:#?}",
            blocks.iter().flat_map(|b| &b.findings).collect::<Vec<_>>()
        );
        assert_eq!(listing(&blocks), kernel_listing(&img.path, path), "{path}");
    }
}

#[test]
fn a_destroyed_directory_block_keeps_exactly_the_entries_the_kernel_still_finds() {
    let img = build_with_mkfs_erofs(&["-b4096"], &tree());
    let geometry = geometry(&img.bytes);
    let wide = kernel_nid(&img.path, "wide");
    let healthy = directory(&img.bytes, &geometry, wide);
    assert!(
        healthy.len() >= 3,
        "fixture: /wide has {} blocks",
        healthy.len()
    );

    let mut bytes = img.bytes.clone();
    let first = healthy[0].offset as usize;
    bytes[first..first + geometry.block_size as usize].fill(0xFF);
    let (image, _guard) = stage_image("dir-damage", &bytes);

    // Every name the healthy directory held, looked up by the kernel.
    let mut script = String::from("while IFS= read -r n; do\n");
    script.push_str(
        "    if i=\"$(stat -c %i \"$MNT/wide/$n\" 2>/dev/null)\"; then printf '%s %s\\n' \"$i\" \"$n\"; fi\n",
    );
    script.push_str("done <<'NAMES'\n");
    for (_, name) in listing(&healthy) {
        script.push_str(&name);
        script.push('\n');
    }
    script.push_str("NAMES\n");
    let found: Listing = kernel_lines(&image, &script)
        .into_iter()
        .map(|line| {
            let (ino, name) = line.split_once(' ').expect("ino name");
            (ino.parse().unwrap(), name.to_string())
        })
        .collect();

    let damaged = directory(&bytes, &geometry, wide);
    assert!(
        !damaged[0].findings.is_empty() && damaged[0].entries.is_empty(),
        "the destroyed block: {:?}",
        damaged[0]
    );
    assert!(
        damaged[1..].iter().all(|b| b.findings.is_empty()),
        "the intact blocks have findings"
    );
    let kept = listing(&damaged);
    assert!(
        !found.is_empty() && found.len() < WIDE,
        "fixture: the kernel finds {} of {WIDE}",
        found.len()
    );
    assert_eq!(
        kept, found,
        "the entries kept are not the names the kernel finds"
    );
}
