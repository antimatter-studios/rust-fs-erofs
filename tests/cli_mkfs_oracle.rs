//! `mkfs.erofs`, the tool, judged by erofs-utils: every image it builds
//! passes `fsck.erofs`, and `dump.erofs` reads the superblock the tool's
//! report describes -- block size, block count, inode count and UUID.
//!
//! The report is read back from the image by this crate's own parser, so
//! only a reader that is not this crate can say the two agree for the
//! right reason. Every tool call runs in the harness VM.

mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{assert_fsck_clean, oracle};

/// The value of `dump.erofs`'s `Filesystem <label>:` line.
#[track_caller]
fn dump_field(dump: &str, label: &str) -> String {
    let prefix = format!("Filesystem {label}:");
    dump.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| panic!("dump.erofs printed no `{prefix}` line:\n{dump}"))
}

#[test]
fn every_image_the_tool_builds_passes_fsck_erofs_and_dump_erofs_agrees_with_its_report() {
    let src = scratch("oracle-src");
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("empty".into(), Vec::new()),
        ("one".into(), pattern(1, 1)),
        ("f4096".into(), pattern(4096, 2)),
        ("f4097".into(), pattern(4097, 3)),
        ("big".into(), pattern(1 << 20, 4)),
        ("n".repeat(255), b"long".to_vec()),
        ("a/b/c/d/e/f/g/h/deep".into(), pattern(3000, 5)),
    ];
    for i in 0..300 {
        files.push((
            format!("wide/entry-with-a-longish-name-{i}"),
            i.to_string().into_bytes(),
        ));
    }
    let borrowed: Vec<(&str, Vec<u8>)> =
        files.iter().map(|(p, b)| (p.as_str(), b.clone())).collect();
    write_tree(&src, &borrowed);
    let src = src.display().to_string();

    // Up to the guest's page size and no further: erofs-utils refuses a
    // larger block ("blksize 65536 isn't supported on this platform") on
    // the x86_64 guest, whatever wrote the image.
    for bs in ["512", "1024", "4096"] {
        let img = image_path(&format!("oracle-b{bs}"));
        let report = stdout(&ok(tool("mkfs.erofs").args(["-q", "-b", bs, &img, &src])));
        assert_fsck_clean(&img, &format!("mkfs.erofs -b {bs}"));

        let out = oracle("dump.erofs").arg(&img).output();
        assert!(out.status.success(), "dump.erofs {img}: {}", stderr(&out));
        let dump = stdout(&out);
        assert_eq!(dump_field(&dump, "blocksize"), bs, "-b {bs}");
        assert_eq!(
            dump_field(&dump, "blocksize"),
            json_field(&report, "block_size")
        );
        assert_eq!(
            dump_field(&dump, "blocks"),
            json_field(&report, "blocks"),
            "-b {bs}"
        );
        assert_eq!(
            dump_field(&dump, "inode count"),
            json_field(&report, "inodes"),
            "-b {bs}"
        );
        assert_eq!(
            dump_field(&dump, "UUID"),
            json_field(&report, "uuid"),
            "-b {bs}"
        );
        // 307 files and 10 directories, the symlink-free tree above.
        assert_eq!(json_field(&report, "inodes"), "317", "-b {bs}");
    }
}
