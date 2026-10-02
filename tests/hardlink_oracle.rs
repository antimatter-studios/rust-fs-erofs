//! Link counts and inode identity, read against images `mkfs.erofs`
//! built and the kernel mounts.
//!
//! EROFS has no separate link records: a hard link is two directory
//! entries naming the same nid, and the count is the inode's `i_nlink`.
//! That field is 16 bits at offset 6 of a compact (32-byte) inode and
//! 32 bits at offset 44 of an extended (64-byte) one, and a directory's
//! count is the number of subdirectories plus two. Nothing in this suite
//! compared any of that with anything but this crate's own writer: the
//! oracle tier asserts only that `fsck.erofs` accepts our writer's
//! directory counts, and the kernel tier's report has no link counts in
//! it at all.
//!
//! So in the harness guest the kernel makes the links — three names for
//! one file across two directories, a hard-linked symlink, a hard-linked
//! FIFO, and one file with 301 names, so a count wider than a byte —
//! `mkfs.erofs` builds the image, once forcing every inode compact and
//! once extended, and the mounted kernel reports every path's link count
//! and inode number. This crate must agree on every link count, and must
//! group the paths into the same inodes the kernel does: the same
//! partition, compared as sets of paths rather than by number, because
//! what a stable identity means is that two names are one file.

mod common;

use fs_erofs::{Filesystem, Inode};
use fs_erofs_test_support::{guest_kernel_link_report, mkfs_from_guest_tree, ScratchDir};
use std::collections::{BTreeMap, BTreeSet};

/// The tree, staged by the kernel in the guest (see `mkfs_from_guest_tree`).
const STAGE: &str = r#"printf 'one\n' > a
ln a b
mkdir d
ln a d/c
printf 'solo\n' > solo
mkdir -p tree/x tree/y tree/z/deeper
ln -s a sym
ln -P sym sym-link
mkfifo fifo
ln fifo fifo-link
printf 'many\n' > many
mkdir many.d
for i in $(seq 1 300); do ln many "many.d/$i"; done"#;

/// Every path in the image, as `find -printf '%P'` names it with the root
/// as `.`, with its inode.
fn walk(fs: &Filesystem, dir: &Inode, prefix: &str, out: &mut Vec<(String, Inode)>) {
    for entry in fs.read_dir(dir).expect("read_dir") {
        let name = String::from_utf8(entry.name.clone()).expect("utf-8 name");
        if name == "." || name == ".." {
            continue;
        }
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let inode = fs
            .read_inode(entry.nid)
            .unwrap_or_else(|error| panic!("{path}: inode {}: {error:?}", entry.nid));
        if inode.is_dir() {
            walk(fs, &inode, &path, out);
        }
        out.push((path, inode));
    }
}

/// The paths grouped by the identity `key` gives them, as a set of sets.
fn classes<K: Ord>(paths: impl Iterator<Item = (String, K)>) -> BTreeSet<BTreeSet<String>> {
    let mut by_key: BTreeMap<K, BTreeSet<String>> = BTreeMap::new();
    for (path, key) in paths {
        by_key.entry(key).or_default().insert(path);
    }
    by_key.into_values().collect()
}

/// Build the tree with `mkfs.erofs <layout>`, then compare link counts
/// and identities with the kernel. `inode_size` is the on-disk size every
/// inode must have, which proves the layout under test is the one asked
/// for.
fn links_agree_with_the_kernel(label: &str, layout: &str, inode_size: u8) {
    let dir = ScratchDir::new(label);
    let image_path = dir.join("links.img");
    let built = mkfs_from_guest_tree(&image_path, STAGE, &["-b4096", layout]);
    assert!(
        built.status.success(),
        "[{label}] staging the tree and running mkfs.erofs {layout} in the guest failed:\n{}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let kernel = guest_kernel_link_report(image_path.to_str().expect("utf-8 path"), label);
    let kernel_field = |kind: &str, path: &str| -> String {
        kernel
            .get(&(kind.to_string(), path.to_string()))
            .unwrap_or_else(|| panic!("[{label}] the kernel reported no {kind} for {path}"))
            .clone()
    };

    // THE KERNEL'S SIDE HAS TO HOLD WHAT THE SUITE IS ABOUT: a tree with
    // no shared inodes agrees with a reader that never shares one.
    for (path, want) in [("a", "3"), ("many", "301"), ("sym", "2"), ("fifo", "2")] {
        assert_eq!(
            kernel_field("links", path),
            want,
            "[{label}] kernel link count of {path}"
        );
    }
    assert_eq!(kernel_field("ino", "a"), kernel_field("ino", "d/c"));

    let fs = common::open_image(std::fs::read(&image_path).expect("read the built image"));
    let root = fs.root_inode().expect("root inode");
    let mut paths = Vec::new();
    walk(&fs, &root, "", &mut paths);
    paths.push((".".to_string(), root));

    let wrong_layout: Vec<&str> = paths
        .iter()
        .filter(|(_, inode)| inode.on_disk_size != inode_size)
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(
        wrong_layout.is_empty(),
        "[{label}] mkfs.erofs {layout} was to make every inode {inode_size} bytes; these are \
         not: {wrong_layout:?}"
    );

    // Every path the kernel listed, this crate listed, and no other.
    let ours_paths: BTreeSet<&str> = paths.iter().map(|(p, _)| p.as_str()).collect();
    let kernel_paths: BTreeSet<&str> = kernel
        .keys()
        .filter(|(kind, _)| kind == "links")
        .map(|(_, path)| path.as_str())
        .collect();
    assert_eq!(ours_paths, kernel_paths, "[{label}] the paths differ");

    let wrong_counts: Vec<String> = paths
        .iter()
        .filter_map(|(path, inode)| {
            let theirs = kernel_field("links", path);
            (theirs != inode.nlink.to_string())
                .then(|| format!("  {path}: kernel {theirs}, ours {}", inode.nlink))
        })
        .collect();
    assert!(
        wrong_counts.is_empty(),
        "[{label}] this crate and the kernel disagree on {} link count(s):\n{}",
        wrong_counts.len(),
        wrong_counts.join("\n")
    );

    let ours_classes = classes(paths.iter().map(|(path, inode)| (path.clone(), inode.nid)));
    let kernel_classes = classes(
        paths
            .iter()
            .map(|(path, _)| (path.clone(), kernel_field("ino", path))),
    );
    assert_eq!(
        ours_classes, kernel_classes,
        "[{label}] this crate groups the paths into different inodes from the kernel"
    );
    println!(
        "[links] {label}: {} paths, {} inodes, every count and identity agrees with the kernel",
        paths.len(),
        ours_classes.len()
    );
}

#[test]
fn link_counts_in_compact_inodes_agree_with_the_kernel() {
    links_agree_with_the_kernel("links-compact", "-Eforce-inode-compact", 32);
}

#[test]
fn link_counts_in_extended_inodes_agree_with_the_kernel() {
    links_agree_with_the_kernel("links-extended", "-Eforce-inode-extended", 64);
}
