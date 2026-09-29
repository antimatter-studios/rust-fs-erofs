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

use common::{build_with_mkfs_erofs, dir, file, mixed_run_files};
use fs_erofs::mkfs;
use fs_erofs_test_support::{fixture, guest_kernel_refusal, guest_kernel_report, sha256_hex};

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

/// THE C ABI's READLINK AGAINST THE KERNEL'S, on an image this crate did
/// not write.
///
/// `/link` in the fixture tree is made by `ln -s` in
/// test-disks/guest-build-images.sh and packed by `mkfs.erofs`; Linux
/// mounts the image and `readlink(1)` reads the target back. The C ABI
/// must return that target's length -- the family contract, `readlink(2)`'s
/// count -- and write those same bytes followed by a NUL. Two images: the
/// default layout, and 512-byte blocks, where the inode sits differently
/// against the block boundary.
#[test]
fn capi_readlink_agrees_with_the_kernel_on_mkfs_erofs_images() {
    use fs_erofs::capi::{fs_erofs_mount, fs_erofs_readlink, fs_erofs_umount};
    use std::ffi::{c_char, CString};

    for name in ["plain", "b512"] {
        let image = fixture(env!("CARGO_MANIFEST_DIR"), &format!("erofs-{name}.img"));
        let report = guest_kernel_report(&image, &format!("erofs-{name}.img /link"));
        let want = report
            .get(&("target".to_string(), "link".to_string()))
            .unwrap_or_else(|| panic!("erofs-{name}.img: the kernel reported no /link target"))
            .clone();
        assert!(
            !want.is_empty(),
            "erofs-{name}.img: the kernel read an empty target"
        );

        let cimage = CString::new(image.clone()).unwrap();
        let fs = unsafe { fs_erofs_mount(cimage.as_ptr()) };
        assert!(
            !fs.is_null(),
            "mount {image}: {}",
            common::capi_last_error()
        );
        let mut buf: Vec<c_char> = vec![0x7F; 4096];
        let n = unsafe {
            fs_erofs_readlink(
                fs,
                CString::new("/link").unwrap().as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        unsafe { fs_erofs_umount(fs) };
        assert_eq!(
            n,
            want.len() as i32,
            "erofs-{name}.img: fs_erofs_readlink returns the kernel's target length ({}): {}",
            want.len(),
            common::capi_last_error()
        );
        assert_eq!(
            buf[want.len()],
            0,
            "erofs-{name}.img: the target is NUL-terminated"
        );
        assert_eq!(
            String::from_utf8(common::cchar_field_to_bytes(&buf)).unwrap(),
            want,
            "erofs-{name}.img: the C ABI and the kernel disagree on /link"
        );
    }
}

/// Build `mixed_run_files` with `mkfs.erofs <args>` in the guest, mount
/// it, and return the files the kernel reads differently from the bytes
/// they were built from, the number it read right, and the image.
fn kernel_misreads(args: &[&str]) -> (Vec<String>, usize, Vec<u8>) {
    let files = mixed_run_files();
    let entries: Vec<(&str, mkfs::Node)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), file(data)))
        .collect();
    let img = build_with_mkfs_erofs(args, &dir(entries));
    let image = img.path.to_str().expect("utf-8 image path");
    let report = guest_kernel_report(image, &format!("mkfs.erofs {}", args.join(" ")));
    let mut wrong = Vec::new();
    let mut read = 0;
    for (name, data) in &files {
        match report.get(&("sha256".to_string(), name.clone())) {
            Some(sha) if *sha == sha256_hex(data) => read += 1,
            Some(_) => wrong.push(name.clone()),
            None => wrong.push(format!("{name} (not in the mounted tree)")),
        }
    }
    (wrong, read, img.bytes)
}

/// The kernel reads a deduplicated image as the tree it was built from
/// (#125).
///
/// This crate's side of dedupe is `oracle_dedupe_round_trip` in
/// tests/oracle_compat.rs, against the files' own bytes. This is the
/// other half: that the images `mkfs.erofs` writes for those shapes
/// really hold those bytes by the in-kernel driver's reading, so a
/// defect in the writer cannot pass for one in the reader or hide behind
/// it.
#[test]
fn the_kernel_reads_a_deduplicated_image_as_its_source() {
    for args in [
        &["-b4096", "-zlz4hc", "-Efragments,ztailpacking,dedupe"][..],
        // LZ4 only: the guest's kernel refuses to mount a MicroLZMA
        // image (`Operation not supported`), so an LZMA shape here would
        // test the kernel's configuration rather than the image.
        &["-b4096", "-zlz4", "-C65536", "-Efragments,dedupe"][..],
    ] {
        let (wrong, read, _) = kernel_misreads(args);
        assert!(
            wrong.is_empty(),
            "{args:?}: the kernel reads {} files differently from their source: {wrong:?}",
            wrong.len()
        );
        assert_eq!(read, mixed_run_files().len(), "{args:?}: files read");
    }
}

/// On `-Eall-fragments,ztailpacking`, the files this crate refuses are
/// exactly the files the kernel misreads (#125).
///
/// erofs-utils 1.9.1 writes the file packed into that image's inline tail
/// so that no reader following the format gets its bytes back: the Linux
/// 6.1 driver reads `f39.bin` differently from its source, and so does
/// `fsck.erofs --extract`. This crate refuses it. Equality both ways is
/// the point: a file the kernel reads right that this crate refuses is a
/// refusal too many, and one the kernel misreads that this crate hands
/// back is a wrong answer given without an error.
#[test]
fn the_kernel_misreads_exactly_the_files_this_crate_refuses() {
    let (kernel_wrong, _, image) =
        kernel_misreads(&["-b4096", "-zlz4hc", "-Eall-fragments,ztailpacking"]);
    let fs = common::open_image(image);
    let mut refused = Vec::new();
    for (name, data) in mixed_run_files() {
        let inode = fs.lookup_path(&format!("/{name}")).expect("lookup");
        let mut buf = vec![0u8; data.len()];
        match fs.read_file(&inode, 0, &mut buf) {
            Err(_) => refused.push(name),
            Ok(()) => assert!(buf == data, "{name}: this crate read wrong bytes"),
        }
    }
    assert!(
        !kernel_wrong.is_empty(),
        "fixture: the kernel read every file right, so there is nothing to refuse -- \
         if mkfs.erofs now writes this shape correctly, this crate should read it"
    );
    assert_eq!(
        refused, kernel_wrong,
        "the files this crate refuses must be the files the kernel misreads"
    );
}
