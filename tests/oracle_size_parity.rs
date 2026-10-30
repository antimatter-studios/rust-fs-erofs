//! The builder's images are as small as `mkfs.erofs -zlz4hc,12 -C 32768`'s,
//! within 2%: the images it replaces `mkfs.erofs` for have to fit fixed-size
//! partitions.
//!
//! The tree mixes what a system partition holds: many small text files,
//! larger ones, binaries that compress in part, archives that do not
//! compress at all, directories of every size, and symlinks. Against
//! erofs-utils 1.9.4, this tree built 0.08% larger; the Android 37
//! emulator's own partitions, rebuilt from their trees, within half a
//! percent (vendor 0.37%, system 0.31%, product 0.24%). 2% leaves room for
//! erofs-utils versions to differ. Before inodes filled the room earlier
//! metadata blocks were left with, this tree built 6% larger.

mod common;

use common::{build_with_mkfs_erofs, dir, file};
use fs_erofs::mkfs::{self, BuildOptions, Compression, Node, NodeMeta, DEFAULT_SYMLINK_MODE};

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn text(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "system", "vendor", "service", "class", "user", "group", "oneshot", "disabled", "socket",
        "property", "trigger", "start", "write", "chmod", "0755", "/data",
    ];
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        let r = xorshift(&mut state);
        out.extend_from_slice(WORDS[(r % 16) as usize].as_bytes());
        out.push(if r.is_multiple_of(7) { b'\n' } else { b' ' });
    }
    out.truncate(len);
    out
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len).map(|_| xorshift(&mut state) as u8).collect()
}

/// A binary: stretches of code-like noise between tables of text.
fn binary(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut i = 0;
    while out.len() < len {
        let chunk = if i % 2 == 0 {
            noise(6000, seed + i)
        } else {
            text(9000, seed + i)
        };
        out.extend(chunk);
        i += 1;
    }
    out.truncate(len);
    out
}

fn tree() -> Node<'static> {
    let mut top: Vec<(String, Node)> = Vec::new();
    for d in 0..40u64 {
        let mut entries: Vec<(String, Node)> = Vec::new();
        for f in 0..(5 + d * 3) {
            let seed = d * 1000 + f;
            entries.push((
                format!("conf-{f:03}.rc"),
                file(&text(200 + (seed as usize * 37) % 6000, seed)),
            ));
        }
        entries.push((
            format!("lib{d}.so"),
            file(&binary(50_000 + d as usize * 7_919, d)),
        ));
        entries.push((
            "link".to_string(),
            Node::Symlink {
                mode: DEFAULT_SYMLINK_MODE,
                target: format!("../d{:02}/lib{d}.so", (d + 1) % 40),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ));
        top.push((
            format!("d{d:02}"),
            dir(entries
                .iter()
                .map(|(n, e)| (n.as_str(), clone(e)))
                .collect()),
        ));
    }
    for a in 0..6u64 {
        top.push((
            format!("app{a}.apk"),
            file(&noise(300_000 + a as usize * 50_021, 99 + a)),
        ));
    }
    top.push(("big.txt".to_string(), file(&text(3 << 20, 7))));
    dir(top.iter().map(|(n, e)| (n.as_str(), clone(e))).collect())
}

/// `Node` holds its file content by value and is not `Clone`; the
/// fixtures here are all directories, files and symlinks.
fn clone(node: &Node<'static>) -> Node<'static> {
    match node {
        Node::Dir {
            mode,
            entries,
            meta,
            xattrs,
        } => Node::Dir {
            mode: *mode,
            entries: entries.iter().map(|(k, v)| (k.clone(), clone(v))).collect(),
            meta: *meta,
            xattrs: xattrs.clone(),
        },
        Node::File {
            mode,
            data,
            meta,
            xattrs,
        } => Node::File {
            mode: *mode,
            data: data.clone(),
            meta: *meta,
            xattrs: xattrs.clone(),
        },
        Node::Symlink {
            mode,
            target,
            meta,
            xattrs,
        } => Node::Symlink {
            mode: *mode,
            target: target.clone(),
            meta: *meta,
            xattrs: xattrs.clone(),
        },
        _ => unreachable!("the fixture has only directories, files and symlinks"),
    }
}

#[test]
fn an_lz4hc_image_is_within_two_percent_of_mkfs_erofs() {
    let theirs = build_with_mkfs_erofs(
        &["-zlz4hc,12", "-C", "32768", "-T", "1230768000", "-b4096"],
        &tree(),
    );
    let options = BuildOptions {
        build_time: 1_230_768_000,
        mtime_is_build_time: true,
        share_xattrs: true,
        compression: Some(Compression::LZ4HC_12_32K),
        ..BuildOptions::default()
    };
    let ours = mkfs::build_image_with(tree(), 12, options).unwrap();
    let (ours_len, theirs_len) = (ours.len() as f64, theirs.bytes.len() as f64);
    assert!(
        ours_len <= theirs_len * 1.02,
        "this crate built {ours_len} bytes, mkfs.erofs {theirs_len}: {:+.2}%",
        (ours_len / theirs_len - 1.0) * 100.0
    );
}
