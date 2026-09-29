//! `fs.erofs`, the multi-call binary's reader, run as a user runs it, on
//! images this crate builds: ls, read, get/info, the verbs a read-only
//! format refuses, `--offset`, and damaged images refused with a
//! structured error and nothing on stdout.
//!
//! Whether erofs-utils and the kernel agree, on images erofs-utils built
//! with every compressor, is the oracle and kernel tiers' question
//! (tests/cli_fs_oracle.rs, tests/cli_fs_kernel.rs). No fixture, no VM:
//! the unit tier.

mod cli_support;
mod common;

use cli_support::*;
use common::{dir, file, symlink};
use fs_erofs::mkfs;
use std::process::Output;

/// An image of a small tree with a symlink in it, built by the library
/// (the `mkfs.erofs` tool leaves symlinks out), written to a file.
fn image() -> (String, Vec<u8>) {
    let tree = dir(vec![
        ("hello.txt", file(b"hi\n")),
        ("empty", file(b"")),
        ("big", file(&pattern(20_000, 1))),
        (
            "sub",
            dir(vec![("deep", dir(vec![("leaf", file(&pattern(5000, 2)))]))]),
        ),
        ("link", symlink("hello.txt")),
    ]);
    let bytes = mkfs::build_image(tree, 12).unwrap();
    let path = image_path("fs");
    std::fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

fn run(image: &str, args: &[&str]) -> Output {
    tool("fs.erofs")
        .arg(image)
        .args(args)
        .output()
        .expect("spawn fs.erofs")
}

/// The entry named `name` in an `ls` report, as its text.
#[track_caller]
fn entry<'a>(listing: &'a str, name: &str) -> &'a str {
    let at = listing
        .find(&format!("\"name\": \"{name}\""))
        .unwrap_or_else(|| panic!("{name} not listed:\n{listing}"));
    &listing[at..at + listing[at..].find('}').unwrap()]
}

/// A failure: the status, nothing on stdout, and the JSON error.
#[track_caller]
fn refused(out: &Output, code: i32) -> String {
    assert_eq!(out.status.code(), Some(code), "{}", stderr(out));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(out));
    let err = stderr(out);
    assert!(
        err.starts_with("{\"error\": \"")
            && err.trim_end().ends_with(&format!("\"code\": {code}}}")),
        "{err}"
    );
    err
}

#[test]
fn ls_lists_a_directory_by_name_with_every_field_typed() {
    let (img, _) = image();
    let listing = stdout(&ok(tool("fs.erofs").args([&img, "ls"])));
    let names: Vec<String> = listing
        .split("\n  {")
        .skip(1)
        .map(|e| json_field(e, "name"))
        .collect();
    assert_eq!(
        names,
        ["big", "empty", "hello.txt", "link", "sub"],
        "{listing}"
    );
    let hello = entry(&listing, "hello.txt");
    assert_eq!(json_field(hello, "type"), "file");
    assert_eq!(json_field(hello, "size"), "3");
    assert_eq!(json_field(hello, "mode"), "0644");
    assert!(json_field(hello, "mtime").parse::<u64>().is_ok(), "{hello}");
    assert!(json_field(hello, "inode").parse::<u64>().is_ok(), "{hello}");
    assert_eq!(json_field(entry(&listing, "sub"), "type"), "dir");
    let link = entry(&listing, "link");
    assert_eq!(json_field(link, "type"), "symlink");
    assert_eq!(json_field(link, "target"), "hello.txt");
    assert!(!listing.contains("\"name\": \".\""), "{listing}");
}

#[test]
fn ls_of_a_file_is_that_one_entry_and_text_is_for_a_person() {
    let (img, _) = image();
    let one = stdout(&ok(tool("fs.erofs").args([&img, "ls", "/sub/deep/leaf"])));
    assert_eq!(one.matches("\"name\"").count(), 1, "{one}");
    assert_eq!(json_field(entry(&one, "leaf"), "size"), "5000");
    let text = stdout(&ok(tool("fs.erofs").args([&img, "ls", "--text", "/"])));
    assert_eq!(text.lines().count(), 5, "{text}");
    assert!(text.contains(" link -> hello.txt"), "{text}");
    assert!(!text.contains('{'), "{text}");
}

#[test]
fn read_writes_each_file_s_exact_bytes_to_stdout_or_a_file() {
    let (img, _) = image();
    for (path, want) in [
        ("/hello.txt", b"hi\n".to_vec()),
        ("/empty", Vec::new()),
        ("/big", pattern(20_000, 1)),
        ("/sub/deep/leaf", pattern(5000, 2)),
    ] {
        let out = ok(tool("fs.erofs").args([&img, "read", path]));
        assert!(out.stdout == want, "{path}: the bytes read differ");
    }
    let dest = image_path("read-o");
    let out = ok(tool("fs.erofs").args([&img, "read", "/big", "-o", &dest]));
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), pattern(20_000, 1));
}

#[test]
fn read_refuses_what_is_not_a_regular_file() {
    let (img, _) = image();
    assert!(refused(&run(&img, &["read", "/sub"]), 1).contains("is a directory"));
    assert!(refused(&run(&img, &["read", "/link"]), 1).contains("is a symlink to hello.txt"));
    assert!(refused(&run(&img, &["read", "/missing"]), 1).contains("not found"));
}

#[test]
fn get_reports_the_canonical_keys_and_erofs_own() {
    let (img, bytes) = image();
    let fs = common::open_image(bytes);
    let sb = fs.superblock();
    let get = stdout(&ok(tool("fs.erofs").args([&img, "get"])));
    assert_eq!(json_field(&get, "fs"), "erofs");
    assert_eq!(json_field(&get, "free_bytes"), "0");
    assert_eq!(json_field(&get, "block_size"), "4096");
    assert_eq!(json_field(&get, "dirty"), "false");
    assert_eq!(
        json_field(&get, "total_bytes"),
        (u64::from(sb.blocks) * 4096).to_string()
    );
    assert_eq!(json_field(&get, "inode_count"), sb.inos.to_string());
    assert_eq!(json_field(&get, "total_blocks"), sb.blocks.to_string());
    assert!(get.contains("\"label\": "), "{get}");
    let info = stdout(&ok(tool("fs.erofs").args([&img, "info"])));
    assert_eq!(info, get, "info and get differ");
    let bs = ok(tool("fs.erofs").args([&img, "get", "block_size", "--text"]));
    assert_eq!(stdout(&bs), "4096\n");
    let uuid = stdout(&ok(tool("fs.erofs").args([
        &img,
        "get",
        "erofs.uuid",
        "--text",
    ])));
    assert_eq!(uuid.trim().len(), 36, "{uuid}");
    assert!(refused(&run(&img, &["get", "no_such_key"]), 2).contains("the keys are fs, label"));
}

#[test]
fn every_verb_that_would_change_the_image_is_refused_as_read_only() {
    let (img, bytes) = image();
    for verb in [
        &["write", "/new"][..],
        &["mkdir", "/newdir"],
        &["set", "label", "X"],
        &["resize", "1G", "--force"],
    ] {
        let err = refused(&run(&img, verb), 3);
        assert!(err.contains("EROFS is read-only"), "{verb:?}: {err}");
    }
    assert!(
        std::fs::read(&img).unwrap() == bytes,
        "a refused verb changed the image"
    );
}

#[test]
fn offset_reaches_an_image_embedded_in_a_larger_one() {
    let (_, bytes) = image();
    let mut padded = vec![0u8; 1 << 20];
    padded.extend_from_slice(&bytes);
    let img = image_path("embedded");
    std::fs::write(&img, padded).unwrap();
    let out = ok(tool("fs.erofs").args(["--offset", "1048576", &img, "read", "/hello.txt"]));
    assert_eq!(out.stdout, b"hi\n");
    let listing = stdout(&ok(
        tool("fs.erofs").args([&img, "ls", "/", "--offset", "1048576"])
    ));
    entry(&listing, "sub");
    refused(&run(&img, &["ls"]), 1);
    assert!(refused(&run(&img, &["--offset", "999999999", "ls"]), 1).contains("past the end"));
}

#[test]
fn a_missing_or_foreign_or_damaged_image_is_a_structured_failure() {
    let (_, bytes) = image();
    assert!(refused(&run(&image_path("never-created"), &["ls"]), 1).contains("open "));

    let zeros = image_path("zeros");
    std::fs::write(&zeros, vec![0u8; 8192]).unwrap();
    assert!(refused(&run(&zeros, &["get"]), 1).contains("is not a readable EROFS image"));

    // One byte of the superblock's UUID changed: the checksum over it no
    // longer matches.
    let mut bad = bytes.clone();
    bad[1024 + 0x30] ^= 0xFF;
    let path = image_path("bad-checksum");
    std::fs::write(&path, bad).unwrap();
    refused(&run(&path, &["ls"]), 1);

    // Cut after the superblock, before the inodes: nothing below the root
    // can be reached. And cut short of the last block: the file whose data
    // is there reads nothing to stdout rather than part of itself.
    let cut = image_path("truncated");
    std::fs::write(&cut, &bytes[..2048]).unwrap();
    for verb in [&["ls", "/sub/deep"][..], &["read", "/big"], &["get"]] {
        refused(&run(&cut, verb), 1);
    }
    let short = image_path("short");
    std::fs::write(&short, &bytes[..bytes.len() - 4096]).unwrap();
    let failed: Vec<&str> = ["/hello.txt", "/big", "/sub/deep/leaf"]
        .into_iter()
        .filter(|path| {
            let out = run(&short, &["read", path]);
            out.status.code() == Some(1) && out.stdout.is_empty()
        })
        .collect();
    assert!(
        !failed.is_empty(),
        "every file read from an image short of its last block"
    );
}

#[test]
fn no_target_or_no_verb_is_usage() {
    let (img, _) = image();
    for args in [&[][..], &[img.as_str()][..]] {
        let out = tool("fs.erofs").args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
