//! `mkfs.erofs`, the tool, judged by THE KERNEL: the images it builds from a
//! source tree, uncompressed and with each compressor, are loop-mounted by
//! Linux in the harness guest. Every file the kernel reads back hashes to
//! its source, the symlink and the FIFO are what they were, and the names of
//! the hardlinked file are one inode with that many links.

mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{guest_kernel_link_report, guest_kernel_report, sha256_hex};

#[test]
fn the_kernel_reads_back_every_file_the_tool_put_in_its_image() {
    let src = scratch("kernel-src");
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one", pattern(1, 1)),
        ("f4096", pattern(4096, 2)),
        ("f4097", pattern(4097, 3)),
        ("big", pattern(1 << 20, 4)),
        (
            "text",
            b"the quick brown fox jumps over the lazy dog\n".repeat(6000),
        ),
        ("a/b/c/d/deep", pattern(3000, 5)),
    ];
    write_tree(&src, &files);
    std::os::unix::fs::symlink("one", src.join("link")).unwrap();
    std::fs::hard_link(src.join("big"), src.join("a/hard")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(src.join("pipe"))
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo: {made}");
    // `big` is met under two names; the report counts one file and one link.
    let source = src.display().to_string();

    for (tag, compression) in [
        ("kernel", &[][..]),
        ("kernel-lz4", &["-z", "lz4"][..]),
        ("kernel-lz4hc", &["-z", "lz4hc,12"][..]),
    ] {
        let img = image_path(tag);
        let report = stdout(&ok(tool("mkfs.erofs")
            .arg("-q")
            .args(compression)
            .args([&img, &source])));
        assert!(report.contains("\"skipped\": []"), "{tag}: {report}");

        let seen = guest_kernel_report(&img, tag);
        let value = |kind: &str, path: &str| {
            seen.get(&(kind.to_string(), path.to_string()))
                .map(String::as_str)
        };
        // `big` is also reached as `a/hard`, with the same bytes.
        let big = files.iter().find(|(path, _)| *path == "big").unwrap();
        for (path, bytes) in files
            .iter()
            .map(|(path, bytes)| (*path, bytes))
            .chain([("a/hard", &big.1)])
        {
            assert_eq!(
                value("sha256", path),
                Some(sha256_hex(bytes).as_str()),
                "{tag} /{path}: the kernel read other bytes"
            );
            assert_eq!(
                value("size", path),
                Some(bytes.len().to_string().as_str()),
                "{tag} /{path}"
            );
        }
        for d in ["a", "a/b", "a/b/c", "a/b/c/d"] {
            assert_eq!(value("type", d), Some("directory"), "{tag} /{d}");
        }
        assert_eq!(value("type", "link"), Some("symbolic-link"), "{tag}");
        assert_eq!(value("target", "link"), Some("one"), "{tag}");
        assert_eq!(value("type", "pipe"), Some("fifo"), "{tag}");

        let links = guest_kernel_link_report(&img, tag);
        let field = |kind: &str, path: &str| {
            links
                .get(&(kind.to_string(), path.to_string()))
                .unwrap_or_else(|| panic!("{tag}: the kernel reported no {kind} for {path}"))
        };
        for path in ["big", "a/hard"] {
            assert_eq!(field("links", path), "2", "{tag} /{path}");
        }
        assert_eq!(field("ino", "big"), field("ino", "a/hard"), "{tag}");
        assert_ne!(field("ino", "big"), field("ino", "one"), "{tag}");
    }
}
