mod cli_support;
mod common;

use cli_support::*;
use common::{dir, file, symlink};
use fs_erofs::mkfs;
use std::process::Output;

fn image() -> String {
    let tree = dir(vec![
        ("empty", dir(vec![])),
        ("hello.txt", file(b"hello\n")),
        ("link", symlink("sub")),
        (
            "sub",
            dir(vec![
                ("deep", dir(vec![("leaf", file(b"payload"))])),
                ("note", file(b"note")),
            ]),
        ),
    ]);
    let image = image_path("listing");
    std::fs::write(&image, mkfs::build_image(tree, 12).unwrap()).unwrap();
    image
}

fn listing_paths(output: &Output) -> Vec<String> {
    stdout(output)
        .split("\n  {")
        .skip(1)
        .map(|row| json_field(row, "path"))
        .collect()
}

#[test]
fn recursive_listing_help_uses_the_shared_arguments_and_defaults() {
    let output = ok(tool("fs.erofs").args(["disk.img", "ls", "--help"]));
    let help = stdout(&output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for expected in [
        "Walk depth first, names sorted as bytes; never follow a symlink",
        "How deep --recursive may go (default: 64); a directory's entries are depth 1",
        "How many entries --recursive may list (default: 100000)",
        "-R, --recursive",
        "--max-depth <N>",
        "--limit <N>",
    ] {
        assert!(help.contains(expected), "missing {expected:?}:\n{help}");
    }
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[test]
fn recursive_listing_bounds_require_recursion_and_unsigned_values() {
    for arguments in [
        vec!["--max-depth", "1"],
        vec!["--limit", "1"],
        vec!["-R", "--max-depth", "-1"],
        vec!["--recursive", "--limit", "nope"],
    ] {
        let output = tool("fs.erofs")
            .args(["disk.img", "ls"])
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{}", stdout(&output));
        assert_eq!(json_field(&stderr(&output), "code"), "2");
    }
}

#[test]
fn recursive_listing_reached_bounds_keep_the_prefix_and_exact_fits_succeed() {
    let image = image();
    let before = std::fs::read(&image).unwrap();
    let complete = ok(tool("fs.erofs").arg(&image).args(["ls", "--recursive"]));
    let expected = [
        "/empty",
        "/hello.txt",
        "/link",
        "/sub",
        "/sub/deep",
        "/sub/deep/leaf",
        "/sub/note",
    ];
    assert_eq!(listing_paths(&complete), expected);
    for arguments in [
        vec!["ls", "-R"],
        vec!["ls", "-R", "--max-depth", "3", "--limit", "7"],
        vec!["ls", "-R", "--max-depth", "64", "--limit", "100000"],
    ] {
        let output = ok(tool("fs.erofs").arg(&image).args(arguments));
        assert_eq!(output.stdout, complete.stdout);
        assert!(output.stderr.is_empty(), "{}", stderr(&output));
    }

    for (bound, value, count, stopped) in [
        ("--max-depth", "0", 0, "/empty"),
        ("--max-depth", "1", 4, "/sub/deep"),
        ("--max-depth", "2", 5, "/sub/deep/leaf"),
        ("--limit", "0", 0, "/empty"),
        ("--limit", "1", 1, "/hello.txt"),
        ("--limit", "4", 4, "/sub/deep"),
        ("--limit", "6", 6, "/sub/note"),
    ] {
        for recursive in ["--recursive", "-R"] {
            let output = tool("fs.erofs")
                .arg(&image)
                .args(["ls", recursive, bound, value])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{bound} {value}");
            assert_eq!(listing_paths(&output), expected[..count]);
            assert_eq!(
                stderr(&output),
                format!("{{\"error\": \"{bound} {value} reached at {stopped}\", \"code\": 1}}\n")
            );
        }
    }

    let text = tool("fs.erofs")
        .arg(&image)
        .args(["ls", "-R", "--text", "--limit", "1"])
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(1));
    assert_eq!(stdout(&text).lines().count(), 1);
    assert_eq!(stderr(&text), "fs.erofs: --limit 1 reached at /hello.txt\n");
    assert_eq!(std::fs::read(&image).unwrap(), before);
}

#[test]
fn recursive_listing_zero_bounds_allow_empty_directories_and_depth_zero_leaves() {
    let image = image();
    let empty = ok(tool("fs.erofs").arg(&image).args([
        "ls",
        "-R",
        "--max-depth",
        "0",
        "--limit",
        "0",
        "/empty",
    ]));
    assert_eq!(stdout(&empty).trim(), "[]");
    assert!(empty.stderr.is_empty(), "{}", stderr(&empty));

    for path in ["/hello.txt", "/link"] {
        let zero = tool("fs.erofs")
            .arg(&image)
            .args(["ls", "-R", "--limit", "0", path])
            .output()
            .unwrap();
        assert_eq!(zero.status.code(), Some(1));
        assert_eq!(stdout(&zero).trim(), "[]");
        assert_eq!(
            stderr(&zero),
            format!("{{\"error\": \"--limit 0 reached at {path}\", \"code\": 1}}\n")
        );

        let output = ok(tool("fs.erofs").arg(&image).args([
            "ls",
            "-R",
            "--max-depth",
            "0",
            "--limit",
            "1",
            path,
        ]));
        assert_eq!(listing_paths(&output), [path]);
        assert_eq!(json_field(&stdout(&output), "depth"), "0");
        assert!(output.stderr.is_empty(), "{}", stderr(&output));
    }
}
