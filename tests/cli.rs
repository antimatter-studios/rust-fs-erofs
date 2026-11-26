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
fn links_and_a_fifo_are_written_as_what_they_are() {
    use fs_erofs::FileType;
    let src = scratch("links-src");
    write_tree(
        &src,
        &[
            ("a-real.txt", b"i am real".to_vec()),
            ("d/empty", Vec::new()),
        ],
    );
    std::os::unix::fs::symlink("a-real.txt", src.join("link.txt")).unwrap();
    std::os::unix::fs::symlink("nowhere/at/all", src.join("dangling")).unwrap();
    std::fs::hard_link(src.join("a-real.txt"), src.join("d/hard")).unwrap();
    std::fs::hard_link(src.join("a-real.txt"), src.join("z-hard")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(src.join("pipe"))
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo: {made}");
    let (img, out) = build("links", &src, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = stdout(&out);
    assert!(report.contains("\"skipped\": []"), "{report}");
    assert_eq!(json_field(&report, "files"), "2");
    assert_eq!(json_field(&report, "symlinks"), "2");
    assert_eq!(json_field(&report, "special_files"), "1");
    assert_eq!(json_field(&report, "hard_links"), "2");
    let fs = open_image_path(Path::new(&img));
    let inode = |path: &str| {
        fs.lookup_path(path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
    };
    for (path, target) in [("/link.txt", "a-real.txt"), ("/dangling", "nowhere/at/all")] {
        let link = inode(path);
        assert_eq!(link.file_type(), FileType::Symlink, "{path}");
        assert_eq!(
            fs.read_symlink_target(&link).unwrap(),
            target.as_bytes(),
            "{path}"
        );
    }
    let first = inode("/a-real.txt");
    assert_eq!(first.nlink, 3);
    for path in ["/d/hard", "/z-hard"] {
        assert_eq!(inode(path).nid, first.nid, "{path} is another inode");
        assert_eq!(read_all(&fs, path), b"i am real", "{path}");
    }
    assert_eq!(inode("/pipe").file_type(), FileType::Fifo);
}

#[cfg(unix)]
#[test]
fn a_symlink_target_that_is_not_utf8_is_left_out_and_named_in_the_report() {
    use std::os::unix::ffi::OsStrExt;
    let src = scratch("raw-target-src");
    write_tree(&src, &[("real.txt", b"i am real".to_vec())]);
    let target = std::ffi::OsStr::from_bytes(b"not-\xff-utf8");
    std::os::unix::fs::symlink(target, src.join("raw")).unwrap();
    let (img, out) = build("raw-target", &src, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = stdout(&out);
    assert!(
        report.contains("\"reason\": \"symlink target is not UTF-8\""),
        "{report}"
    );
    assert_eq!(json_field(&report, "symlinks"), "0");
    assert!(
        stderr(&out).contains("warning: left out") && stderr(&out).contains("raw"),
        "{}",
        stderr(&out)
    );
    let fs = open_image_path(Path::new(&img));
    assert_eq!(read_all(&fs, "/real.txt"), b"i am real");
    assert!(fs.lookup_path("/raw").is_err());
}

#[test]
fn each_compressor_makes_a_smaller_image_that_reads_back_the_same() {
    use fs_erofs::DataLayout;
    let src = scratch("compress-src");
    let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog\n".repeat(6000);
    let files = [
        ("text", text),
        ("noise", pattern(20_000, 7)),
        ("empty", Vec::new()),
        ("tiny", b"tiny".to_vec()),
    ];
    write_tree(&src, &files);
    let (plain, out) = build("compress-none", &src, &["-q"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("\"compression\": null"),
        "{}",
        stdout(&out)
    );
    let plain_len = std::fs::metadata(&plain).unwrap().len();
    for (args, named) in [
        (&["-q", "-z", "lz4"][..], "lz4"),
        (&["-q", "-zlz4hc"][..], "lz4hc,9"),
        (&["-q", "--compress", "lz4hc,12"][..], "lz4hc,12"),
    ] {
        let (img, out) = build("compress", &src, args);
        assert!(out.status.success(), "{named}: {}", stderr(&out));
        assert_eq!(json_field(&stdout(&out), "compression"), named);
        let len = std::fs::metadata(&img).unwrap().len();
        assert!(len < plain_len, "{named}: {len} bytes, {plain_len} plain");
        let fs = open_image_path(Path::new(&img));
        let layout = fs.lookup_path("/text").unwrap().format.layout;
        assert!(
            matches!(
                layout,
                DataLayout::Compression | DataLayout::CompressionLegacy
            ),
            "{named}: /text is {layout:?}"
        );
        for (path, bytes) in &files {
            assert_eq!(
                read_all(&fs, &format!("/{path}")),
                *bytes,
                "{named} /{path}"
            );
        }
    }
}

#[test]
fn a_compressor_the_tool_does_not_have_is_usage_and_writes_nothing() {
    let src = scratch("bad-z");
    for value in ["zstd", "lz4hc,13", "lz4hc,0", "lz4,1"] {
        let (img, out) = build("bad-z", &src, &["-z", value]);
        assert_eq!(out.status.code(), Some(2), "-z {value}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "-z {value}");
        assert!(!Path::new(&img).exists(), "-z {value}");
    }
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
