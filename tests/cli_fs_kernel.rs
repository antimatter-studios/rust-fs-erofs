//! `fs.erofs`, the tool, held to THE KERNEL on images erofs-utils made:
//! each is loop-mounted by Linux in the harness guest, and every file the
//! kernel reads hashes to what `fs.erofs read` returns, and every symlink
//! target the kernel reports is the one `fs.erofs ls` lists.

mod cli_support;
mod common;

use cli_support::*;
use common::{build_with_mkfs_erofs, dir, file, symlink};
use fs_erofs_test_support::{guest_kernel_report, sha256_hex};

#[test]
fn the_tool_reads_what_the_kernel_reads_uncompressed_and_lz4() {
    let tree = dir(vec![
        ("empty", file(b"")),
        ("one", file(&pattern(1, 1))),
        ("f4097", file(&pattern(4097, 2))),
        ("big", file(&pattern(300_000, 3))),
        (
            "d",
            dir(vec![("e", dir(vec![("deep", file(&pattern(5000, 4)))]))]),
        ),
        ("link", symlink("one")),
    ]);
    for args in [&["-b4096"][..], &["-b4096", "-zlz4"]] {
        let built = build_with_mkfs_erofs(args, &tree);
        let image = built.path.display().to_string();
        let kernel = guest_kernel_report(&image, &format!("mkfs.erofs {args:?}"));
        let (mut files, mut links) = (0, 0);
        for ((kind, path), value) in &kernel {
            match kind.as_str() {
                "sha256" => {
                    let read = ok(tool("fs.erofs").args([&image, "read", &format!("/{path}")]));
                    assert_eq!(&sha256_hex(&read.stdout), value, "{args:?} /{path}");
                    files += 1;
                }
                "target" => {
                    let listed = stdout(&ok(tool("fs.erofs").args([
                        &image,
                        "ls",
                        &format!("/{path}"),
                    ])));
                    assert_eq!(&json_field(&listed, "target"), value, "{args:?} /{path}");
                    links += 1;
                }
                _ => {}
            }
        }
        // Five regular files and a symlink: two empty maps agree about
        // everything, so the comparison must have compared something.
        assert_eq!((files, links), (5, 1), "{args:?}: {kernel:?}");
    }
}
