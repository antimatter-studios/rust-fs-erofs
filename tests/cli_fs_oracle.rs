//! `fs.erofs`, the tool, reading images this crate did not make:
//! erofs-utils' `mkfs.erofs` builds each one in the harness guest,
//! uncompressed and with every compressor it has, and
//!
//! - every file `fs.erofs read` returns is its source, byte for byte, and
//!   `fs.erofs ls` lists every entry with the source's type, size and
//!   symlink target;
//! - `dump.erofs` agrees with `fs.erofs get` on block size, block count,
//!   inode count and UUID;
//! - an image damaged two ways (its superblock checksum, its length) is
//!   refused by `fsck.erofs` and by the tool, with a structured error and
//!   nothing on stdout.
//!
//! Every tool call runs in the harness VM.

mod cli_support;
mod common;

use cli_support::*;
use common::{build_with_mkfs_erofs, dir, file, symlink};
use fs_erofs_test_support::oracle;

/// The source: every size boundary a 4 KiB block has, a MiB of noise, a
/// long name, deep nesting, a directory that spans blocks, and symlinks.
fn source() -> (fs_erofs::mkfs::Node, Vec<(String, Vec<u8>)>) {
    let long = "n".repeat(255);
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("empty".into(), Vec::new()),
        ("one".into(), pattern(1, 1)),
        ("f4096".into(), pattern(4096, 2)),
        ("f4097".into(), pattern(4097, 3)),
        ("big".into(), pattern(1 << 20, 4)),
        (long.clone(), b"long".to_vec()),
        ("a/b/c/deep".into(), pattern(5000, 5)),
    ];
    let wide: Vec<(String, Vec<u8>)> = (0..300)
        .map(|i| {
            (
                format!("wide/entry-with-a-longish-name-{i}"),
                i.to_string().into_bytes(),
            )
        })
        .collect();
    files.extend(wide.iter().cloned());
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let tree = dir(vec![
        ("empty", file(b"")),
        ("one", file(&pattern(1, 1))),
        ("f4096", file(&pattern(4096, 2))),
        ("f4097", file(&pattern(4097, 3))),
        ("big", file(&pattern(1 << 20, 4))),
        (leak(long), file(b"long")),
        (
            "a",
            dir(vec![(
                "b",
                dir(vec![("c", dir(vec![("deep", file(&pattern(5000, 5)))]))]),
            )]),
        ),
        (
            "wide",
            dir(wide
                .iter()
                .map(|(p, b)| (leak(p.trim_start_matches("wide/").to_string()), file(b)))
                .collect()),
        ),
        ("link", symlink("one")),
        ("deep_link", symlink("a/b/c/deep")),
    ]);
    (tree, files)
}

/// The value of `dump.erofs`'s `Filesystem <label>:` line.
#[track_caller]
fn dump_field(dump: &str, label: &str) -> String {
    let prefix = format!("Filesystem {label}:");
    dump.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| panic!("dump.erofs printed no `{prefix}` line:\n{dump}"))
}

fn check(args: &[&str]) {
    let (tree, files) = source();
    let built = build_with_mkfs_erofs(args, &tree);
    let image = built.path.display().to_string();

    for (path, bytes) in &files {
        let read = ok(tool("fs.erofs").args([&image, "read", &format!("/{path}")]));
        assert!(
            read.stdout == *bytes,
            "{args:?} /{path}: the bytes read are not the source's"
        );
    }
    let root = stdout(&ok(tool("fs.erofs").args([&image, "ls", "/"])));
    for name in [
        "empty",
        "one",
        "f4096",
        "f4097",
        "big",
        "a",
        "wide",
        "link",
        "deep_link",
    ] {
        assert!(
            root.contains(&format!("\"name\": \"{name}\"")),
            "{args:?}: {name} not listed:\n{root}"
        );
    }
    for (link, target) in [("link", "one"), ("deep_link", "a/b/c/deep")] {
        let listed = stdout(&ok(tool("fs.erofs").args([
            &image,
            "ls",
            &format!("/{link}"),
        ])));
        assert_eq!(json_field(&listed, "type"), "symlink", "{args:?} /{link}");
        assert_eq!(json_field(&listed, "target"), target, "{args:?} /{link}");
    }
    let big = stdout(&ok(tool("fs.erofs").args([&image, "ls", "/big"])));
    assert_eq!(json_field(&big, "size"), (1u32 << 20).to_string());
    let wide = stdout(&ok(tool("fs.erofs").args([&image, "ls", "/wide"])));
    assert_eq!(wide.matches("\"name\": ").count(), 300, "{args:?}: /wide");

    let get = stdout(&ok(tool("fs.erofs").args([&image, "get"])));
    let out = oracle("dump.erofs").arg(&image).output();
    assert!(out.status.success(), "dump.erofs {image}: {}", stderr(&out));
    let dump = stdout(&out);
    assert_eq!(
        dump_field(&dump, "blocksize"),
        json_field(&get, "block_size"),
        "{args:?}"
    );
    assert_eq!(
        dump_field(&dump, "blocks"),
        json_field(&get, "total_blocks"),
        "{args:?}"
    );
    assert_eq!(
        dump_field(&dump, "inode count"),
        json_field(&get, "inode_count"),
        "{args:?}"
    );
    assert_eq!(
        dump_field(&dump, "UUID"),
        json_field(&get, "uuid"),
        "{args:?}"
    );
    assert_eq!(json_field(&get, "fs"), "erofs");
    assert_eq!(json_field(&get, "dirty"), "false");
}

#[test]
fn uncompressed() {
    check(&["-b4096"]);
}

#[test]
fn lz4() {
    check(&["-b4096", "-zlz4"]);
}

#[test]
fn lz4hc() {
    check(&["-b4096", "-zlz4hc"]);
}

#[test]
fn lzma() {
    check(&["-b4096", "-zlzma"]);
}

#[test]
fn deflate() {
    check(&["-b4096", "-zdeflate"]);
}

#[test]
fn zstd() {
    check(&["-b4096", "-zzstd"]);
}

#[test]
fn a_damaged_image_is_refused_by_fsck_erofs_and_by_the_tool() {
    let (tree, _) = source();
    let built = build_with_mkfs_erofs(&["-b4096"], &tree);
    let scratch = scratch("oracle-damaged");
    // A byte of the superblock's UUID changed, so the CRC32C over it no
    // longer matches; and the image cut in half.
    let mut bad_checksum = built.bytes.clone();
    bad_checksum[1024 + 0x30] ^= 0xFF;
    let mut cut = built.bytes.clone();
    cut.truncate(built.bytes.len() / 2);
    for (what, bytes) in [
        ("a bad superblock checksum", bad_checksum),
        ("a truncated image", cut),
    ] {
        let path = scratch.join(format!("{}.img", what.replace(' ', "-")));
        std::fs::write(&path, bytes).unwrap();
        let path = path.display().to_string();
        let theirs = oracle("fsck.erofs").arg(&path).output();
        assert!(
            !theirs.status.success(),
            "fsck.erofs accepted {what}:\n{}",
            stdout(&theirs)
        );
        let ours = tool("fs.erofs")
            .args([&path, "read", "/big"])
            .output()
            .unwrap();
        assert_eq!(ours.status.code(), Some(1), "{what}: {}", stderr(&ours));
        assert!(
            ours.stdout.is_empty(),
            "{what}: stdout has {} bytes",
            ours.stdout.len()
        );
        assert!(
            stderr(&ours).starts_with("{\"error\": "),
            "{what}: {}",
            stderr(&ours)
        );
    }
}
