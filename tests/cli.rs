//! `mkfs.erofs`, the multi-call binary's builder, run as a user runs it.
//!
//! Every image is opened again with this crate's own reader, so these
//! prove the tool and the library agree; whether erofs-utils and the
//! kernel agree with both is the oracle tier's question
//! (tests/cli_mkfs_oracle.rs).
//!
//! No fixture, no VM: the unit tier.

mod cli_support;
mod common;

use cli_support::*;
use common::{dir, file, open_image_path};
use fs_erofs::mkfs;
use std::path::Path;

/// `mkfs.erofs ARGS... OUTPUT SOURCE`, with a fresh output path.
fn build(tag: &str, src: &Path, args: &[&str]) -> (String, std::process::Output) {
    let img = image_path(tag);
    let out = tool("mkfs.erofs")
        .args(args)
        .arg(&img)
        .arg(src)
        .output()
        .expect("spawn mkfs.erofs");
    (img, out)
}

fn read_all(fs: &fs_erofs::Filesystem, path: &str) -> Vec<u8> {
    let inode = fs
        .lookup_path(path)
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    buf
}

#[test]
fn help_names_the_arguments_and_carries_examples() {
    for flag in ["--help", "-h"] {
        let out = ok(tool("mkfs.erofs").arg(flag));
        let text = stdout(&out);
        assert!(text.contains("Usage: mkfs.erofs"), "{text}");
        assert!(text.contains("<OUTPUT> <SOURCE>"), "{text}");
        assert!(text.contains("Examples:"), "{text}");
    }
}

#[test]
fn a_missing_source_is_a_structured_failure_and_writes_nothing() {
    let src = scratch("missing-src").join("does-not-exist");
    let (img, out) = build("missing-src", &src, &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let err = stderr(&out);
    assert!(err.contains("\"error\": \"walking "), "{err}");
    assert!(err.contains("\"code\": 1"), "{err}");
    assert!(!Path::new(&img).exists());
}

#[test]
fn a_block_size_the_format_cannot_hold_is_refused_as_usage() {
    let src = scratch("bad-bs");
    for (value, says) in [
        ("99", "not a power of 2 in 512..=65536"),
        ("256", "not a power of 2 in 512..=65536"),
        ("131072", "not a power of 2 in 512..=65536"),
        ("abc", "is not a number of bytes"),
    ] {
        let (img, out) = build("bad-bs", &src, &["--block-size", value]);
        assert_eq!(out.status.code(), Some(2), "--block-size {value}");
        assert!(out.stdout.is_empty(), "--block-size {value}");
        assert!(stderr(&out).contains(says), "{value}: {}", stderr(&out));
        assert!(!Path::new(&img).exists(), "--block-size {value}");
    }
    let out = tool("mkfs.erofs").arg("--block-size").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--block-size"), "{}", stderr(&out));
}

#[test]
fn an_unknown_flag_or_a_missing_argument_is_usage() {
    let out = tool("mkfs.erofs").arg("--unknown-thing").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--unknown-thing"), "{}", stderr(&out));
    let out = tool("mkfs.erofs").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("<OUTPUT>"), "{}", stderr(&out));
}

#[test]
fn label_is_refused_as_not_implemented_and_writes_nothing() {
    let src = scratch("label-src");
    write_tree(&src, &[("a", pattern(10, 1))]);
    for flag in ["-L", "--label"] {
        let (img, out) = build("label", &src, &[flag, "BACKUP"]);
        assert_eq!(out.status.code(), Some(3), "{flag}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{flag}");
        let err = stderr(&out);
        assert!(
            err.contains("\"error\": \"not implemented: --label"),
            "{err}"
        );
        assert!(err.contains("\"code\": 3"), "{err}");
        assert!(!Path::new(&img).exists(), "{flag} wrote {img}");
    }
}

#[test]
fn the_report_is_read_back_from_the_image_it_describes() {
    let src = scratch("report-src");
    write_tree(
        &src,
        &[
            ("a.txt", b"alpha".to_vec()),
            ("b.txt", b"bravo bravo".to_vec()),
            ("sub/c.txt", b"charlie".to_vec()),
        ],
    );
    let (img, out) = build("report", &src, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = stdout(&out);
    let fs = open_image_path(Path::new(&img));
    let sb = fs.superblock();
    assert_eq!(json_field(&report, "image"), img);
    assert_eq!(
        json_field(&report, "bytes"),
        std::fs::metadata(&img).unwrap().len().to_string()
    );
    assert_eq!(json_field(&report, "block_size"), "4096");
    assert_eq!(json_field(&report, "blocks"), sb.blocks.to_string());
    assert_eq!(json_field(&report, "inodes"), sb.inos.to_string());
    assert_eq!(json_field(&report, "files"), "3");
    assert_eq!(json_field(&report, "directories"), "2");
    assert!(report.contains("\"skipped\": []"), "{report}");
    // Progress on stderr, as the tool always printed it.
    assert!(stderr(&out).contains("wrote "), "{}", stderr(&out));
    for (path, want) in [
        ("/a.txt", &b"alpha"[..]),
        ("/b.txt", &b"bravo bravo"[..]),
        ("/sub/c.txt", &b"charlie"[..]),
    ] {
        assert_eq!(read_all(&fs, path), want, "{path}");
    }
}

#[test]
fn text_and_quiet_print_nothing_on_success() {
    let src = scratch("quiet-src");
    write_tree(&src, &[("a", pattern(10, 1))]);
    let (img, out) = build("quiet", &src, &["--text", "-q"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
    assert_eq!(
        read_all(&open_image_path(Path::new(&img)), "/a"),
        pattern(10, 1)
    );
}

#[test]
fn every_block_size_the_reader_supports_round_trips() {
    let src = scratch("bs-src");
    let files = [
        ("empty", Vec::new()),
        ("one", pattern(1, 1)),
        ("f511", pattern(511, 2)),
        ("f513", pattern(513, 3)),
        ("f4097", pattern(4097, 4)),
        ("big", pattern(200_000, 5)),
        ("d/e/deep", pattern(3000, 6)),
    ];
    write_tree(&src, &files);
    for bits in 9..=16u32 {
        let bs = (1u64 << bits).to_string();
        let (img, out) = build(&format!("bs{bs}"), &src, &["-q", "-b", &bs]);
        assert!(out.status.success(), "-b {bs}: {}", stderr(&out));
        assert_eq!(json_field(&stdout(&out), "block_size"), bs);
        let fs = open_image_path(Path::new(&img));
        assert_eq!(fs.superblock().block_size().to_string(), bs);
        for (path, bytes) in &files {
            assert_eq!(
                read_all(&fs, &format!("/{path}")),
                *bytes,
                "-b {bs} /{path}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_source_is_left_out_and_named_in_the_report() {
    let src = scratch("symlink-src");
    write_tree(&src, &[("real.txt", b"i am real".to_vec())]);
    std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();
    let (img, out) = build("symlink", &src, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = stdout(&out);
    assert!(report.contains("\"reason\": \"symlink\""), "{report}");
    assert!(
        report.contains(&format!("\"path\": \"{}\"", src.join("link.txt").display())),
        "{report}"
    );
    assert!(
        stderr(&out).contains("warning: left out") && stderr(&out).contains("link.txt"),
        "{}",
        stderr(&out)
    );
    let fs = open_image_path(Path::new(&img));
    assert_eq!(read_all(&fs, "/real.txt"), b"i am real");
    assert!(fs.lookup_path("/link.txt").is_err());
}

/// SOURCE naming a regular file makes an image whose root holds that one
/// file.
#[test]
fn the_source_can_be_a_regular_file() {
    let src = scratch("single-src");
    write_tree(&src, &[("payload.bin", b"singular".to_vec())]);
    let (img, out) = build("single", &src.join("payload.bin"), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json_field(&stdout(&out), "files"), "1");
    let fs = open_image_path(Path::new(&img));
    assert_eq!(read_all(&fs, "/payload.bin"), b"singular");
}

/// The tool's walk produces the same tree `build_image` makes when handed
/// it directly.
#[test]
fn the_tool_and_the_library_build_the_same_tree() {
    let tree = dir(vec![
        ("one.txt", file(b"one")),
        ("two.txt", file(b"two two")),
    ]);
    let direct = common::open_image(mkfs::build_image(tree, 12).unwrap());
    let src = scratch("same-src");
    write_tree(
        &src,
        &[
            ("one.txt", b"one".to_vec()),
            ("two.txt", b"two two".to_vec()),
        ],
    );
    let (img, out) = build("same", &src, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let from_tool = open_image_path(Path::new(&img));
    for path in ["/one.txt", "/two.txt"] {
        assert_eq!(
            read_all(&from_tool, path),
            read_all(&direct, path),
            "{path}"
        );
    }
}
