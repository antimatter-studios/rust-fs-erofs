//! `mkfs.erofs`, the tool, judged by THE KERNEL: the image it builds from a
//! source tree is loop-mounted by Linux in the harness guest, and every
//! file the kernel reads back hashes to its source. The symlink the tool
//! leaves out is absent from the mount, as its report says.

mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{guest_kernel_report, sha256_hex};

#[test]
fn the_kernel_reads_back_every_file_the_tool_put_in_its_image() {
    let src = scratch("kernel-src");
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one", pattern(1, 1)),
        ("f4096", pattern(4096, 2)),
        ("f4097", pattern(4097, 3)),
        ("big", pattern(1 << 20, 4)),
        ("a/b/c/d/deep", pattern(3000, 5)),
    ];
    write_tree(&src, &files);
    std::os::unix::fs::symlink("one", src.join("link")).unwrap();
    let img = image_path("kernel");
    let report = stdout(&ok(tool("mkfs.erofs").args([
        "-q",
        &img,
        &src.display().to_string(),
    ])));
    assert!(report.contains("\"reason\": \"symlink\""), "{report}");

    let seen = guest_kernel_report(&img, "mkfs.erofs's image");
    for (path, bytes) in &files {
        let key = |kind: &str| (kind.to_string(), path.to_string());
        assert_eq!(
            seen.get(&key("sha256")),
            Some(&sha256_hex(bytes)),
            "/{path}: the kernel read other bytes"
        );
        assert_eq!(
            seen.get(&key("size")),
            Some(&bytes.len().to_string()),
            "/{path}"
        );
    }
    for d in ["a", "a/b", "a/b/c", "a/b/c/d"] {
        assert_eq!(
            seen.get(&("type".to_string(), d.to_string()))
                .map(String::as_str),
            Some("directory"),
            "/{d}"
        );
    }
    assert!(
        !seen.contains_key(&("type".to_string(), "link".to_string())),
        "the symlink the tool left out is in the mount: {seen:?}"
    );
}
