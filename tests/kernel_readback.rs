//! THE KERNEL ORACLE: the real in-kernel EROFS driver reading back what
//! this crate's writer emitted, and refusing what it should refuse.
//!
//! Its own file, because it is its own tier (`chore test:kernel`) and
//! its own kind of failure: `fsck.erofs` reads the format from the same
//! specification this crate does, while Linux is the thing the images
//! are for. The mount happens inside the fs-linux-test-harness VM and
//! nowhere else — see `fs_erofs_test_support::kernel` for why a `sudo
//! mount` from a test was never an answer.

mod common;

use common::{dir, file};
use fs_erofs::mkfs;
use fs_erofs_test_support::{guest_kernel_refusal, guest_kernel_report, sha256_hex};

/// The tree the kernel-mountability check writes, and what every file in
/// it must read back as through the mount.
///
/// Deliberately more than one inode layout: a tail-inlined small file, a
/// file that is exactly one block, a multi-block one, an empty one, a
/// symlink and two levels of directory. A mount proves the superblock
/// parses; only reading every one of these back proves the inodes,
/// directory blocks and data layout the writer emitted are the ones the
/// kernel thinks they are.
fn kernel_mount_sample() -> (Vec<(String, Vec<u8>)>, mkfs::Node) {
    let mut big = Vec::with_capacity(100_000);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    while big.len() < 100_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        big.extend_from_slice(&seed.to_le_bytes());
    }
    big.truncate(100_000);
    let exact_block = vec![0x5Au8; 4096];
    let leaf = vec![0x11u8; 5000];

    let files: Vec<(String, Vec<u8>)> = vec![
        ("/a.bin", vec![0xABu8; 200]),
        ("/big.bin", big.clone()),
        ("/empty", Vec::new()),
        ("/exact_block.bin", exact_block.clone()),
        ("/hello.txt", b"hello\n".to_vec()),
        ("/sub/deeper/leaf.bin", leaf.clone()),
        ("/sub/nested.txt", b"nested\n".to_vec()),
    ]
    .into_iter()
    .map(|(p, b)| (p.to_string(), b))
    .collect();

    let tree = dir(vec![
        ("hello.txt", file(b"hello\n")),
        ("a.bin", file(&[0xABu8; 200])),
        ("empty", file(b"")),
        ("exact_block.bin", file(&exact_block)),
        ("big.bin", file(&big)),
        (
            "link.txt",
            mkfs::Node::Symlink {
                mode: 0o120777,
                target: "hello.txt".to_string(),
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
        (
            "sub",
            dir(vec![
                ("nested.txt", file(b"nested\n")),
                ("deeper", dir(vec![("leaf.bin", file(&leaf))])),
            ]),
        ),
    ]);
    (files, tree)
}

/// THE KERNEL ORACLE: an image emitted by our writer is mountable by the
/// live Linux EROFS driver, and what the kernel then reads out of it is
/// what the writer put in.
///
/// # This used to be a `sudo -n mount` on whatever host ran the tests
///
/// It spent its whole existence returning early: it invoked `mount`
/// unprivileged, matched the refusal against a list of permission-denied
/// wordings, printed "skipping" and returned ok -- so the repository's
/// strongest claim about its writer was answered by a `mount` that was
/// never allowed to run (#117). #121 made it fail in CI, which left it
/// runnable on a Linux runner with passwordless sudo and nowhere else: a
/// developer, and a Mac, still got the skip.
///
/// The mount now happens inside the fs-linux-test-harness VM, which has
/// root, the loop driver and the EROFS module because
/// `scripts/vm-setup.sh` put them there. There is nothing left to be
/// refused and nothing to skip; a mount that does not happen fails.
///
/// # What it proves
///
/// `mount(2)` succeeding proves the superblock is acceptable and nothing
/// more. So after mounting, every file in the tree is read back through
/// the kernel and compared by name, by size and by SHA-256 of its
/// contents, the directories are compared as a set, and the symlink's
/// target is read. A writer defect that produces a mountable image with
/// the wrong bytes in it fails here.
#[test]
fn our_writer_image_kernel_mountable() {
    let (expected_files, tree) = kernel_mount_sample();
    let img_bytes = mkfs::build_image(tree, 12).expect("build_image");
    let (image, _guard) = common::stage_image("kernel-mount", &img_bytes);
    let image = image.to_str().expect("utf-8 image path");
    let report = guest_kernel_report(image, "our writer's image");

    let want: std::collections::BTreeMap<String, Vec<u8>> = expected_files.into_iter().collect();

    let saw_files: Vec<String> = report
        .keys()
        .filter(|(kind, _)| kind == "sha256")
        .map(|(_, path)| format!("/{path}"))
        .collect();
    assert_eq!(
        saw_files,
        want.keys().cloned().collect::<Vec<_>>(),
        "the mounted tree holds different files from the ones the writer was given"
    );

    let saw_dirs: Vec<String> = report
        .iter()
        .filter(|((kind, _), value)| kind == "type" && value.as_str() == "directory")
        .map(|((_, path), _)| format!("/{path}"))
        .collect();
    assert_eq!(
        saw_dirs,
        vec!["/sub".to_string(), "/sub/deeper".to_string()],
        "the mounted tree's directories"
    );

    assert_eq!(
        report
            .get(&("target".to_string(), "link.txt".to_string()))
            .map(String::as_str),
        Some("hello.txt"),
        "the symlink the writer emitted, as the kernel reads it: {report:?}"
    );

    for (path, want_bytes) in &want {
        let key = path.trim_start_matches('/').to_string();
        assert_eq!(
            report.get(&("size".to_string(), key.clone())),
            Some(&want_bytes.len().to_string()),
            "{path}: size through the kernel mount"
        );
        assert_eq!(
            report.get(&("sha256".to_string(), key)),
            Some(&sha256_hex(want_bytes)),
            "{path}: SHA-256 of the bytes the kernel read differs from what the writer wrote"
        );
    }
}

/// THE OTHER HALF OF A MOUNT ORACLE: an image the driver knows is
/// damaged has to be one Linux also rejects.
///
/// Without this, "the kernel mounted it" is only half a claim: a driver
/// that emitted a superblock the kernel accepts unconditionally would
/// pass the test above for the wrong reason. The magic number is
/// overwritten, which is the one field every EROFS reader checks first,
/// so a mount that still succeeds means the guest did not read the image
/// this test wrote.
#[test]
fn a_damaged_superblock_is_refused_by_the_kernel() {
    let (_, tree) = kernel_mount_sample();
    let mut img_bytes = mkfs::build_image(tree, 12).expect("build_image");
    // The superblock begins 1024 bytes in; its first four bytes are the
    // magic (EROFS_SUPER_MAGIC_V1).
    img_bytes[1024..1028].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let (image, _guard) = common::stage_image("kernel-refusal", &img_bytes);
    let said = guest_kernel_refusal(
        image.to_str().expect("utf-8 image path"),
        "a superblock with the magic overwritten",
    );
    assert!(
        !said.trim().is_empty(),
        "the kernel refused the image but said nothing about it"
    );
}
