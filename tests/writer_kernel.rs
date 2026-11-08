//! The builder's images as the real kernel reads them: each test here has
//! `fsck.erofs` accept an image this crate wrote, then mounts it in the
//! harness VM.
//!
//! In a file of their own because `scripts/test-targets.sh` sorts a whole
//! file by what it calls: one kernel call in `oracle_writer.rs` would take
//! all of it out of the oracle tier.

mod common;

use common::{dir, file, open_image_path};
use fs_erofs::mkfs;
use fs_erofs_test_support::oracle;

/// Write `bytes` into the repository's scratch directory, the only tree
/// the guest can see, and return the path plus the guard that deletes it.
fn stage_image(bytes: &[u8]) -> (std::path::PathBuf, fs_erofs_test_support::ScratchDir) {
    common::stage_image("writer", bytes)
}

fn run_fsck(path: &std::path::Path) -> (Option<i32>, String, String) {
    let out = oracle("fsck.erofs").arg(path).output();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Repeated xattrs written once, in the shared area, and referenced from
/// every inode that carries them: `fsck.erofs` accepts the image, and the
/// kernel reads every inode's full set, shared and inline, as this crate
/// does.
#[test]
fn shared_xattrs_are_accepted_by_fsck_and_read_by_the_kernel() {
    use fs_erofs::xattr::ns;
    let label = || {
        mkfs::XattrSpec::new(
            ns::SECURITY,
            b"selinux".to_vec(),
            b"u:object_r:system_file:s0\0".to_vec(),
        )
    };
    let with = |data: &[u8], xattrs: Vec<mkfs::XattrSpec>| mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        meta: mkfs::NodeMeta::default(),
        xattrs,
    };
    let names: Vec<String> = (0..200).map(|i| format!("f{i}")).collect();
    let mut entries: Vec<(&str, mkfs::Node)> = names
        .iter()
        .map(|n| (n.as_str(), with(n.as_bytes(), vec![label()])))
        .collect();
    entries.push((
        "own",
        with(
            b"own",
            vec![
                label(),
                mkfs::XattrSpec::new(ns::USER, b"only".to_vec(), b"here".to_vec()),
            ],
        ),
    ));
    let options = mkfs::BuildOptions {
        share_xattrs: true,
        ..mkfs::BuildOptions::default()
    };
    let img = mkfs::build_image_with(dir(entries), 12, options).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    let report = fs_erofs_test_support::guest_kernel_read(
        path.to_str().expect("utf-8 path"),
        r#"python3 - "$MNT" <<'PY'
import os, sys
for name in sorted(os.listdir(sys.argv[1])):
    p = os.path.join(sys.argv[1], name)
    for x in sorted(os.listxattr(p)):
        print(name, x, os.getxattr(p, x).hex())
PY"#,
    );
    assert!(
        report.status.success(),
        "the kernel could not read the image:\n{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let kernel = String::from_utf8(report.stdout).expect("utf-8 report");

    let fs = open_image_path(&path);
    let root = fs.root_inode().unwrap();
    let mut ours = String::new();
    let mut listed: Vec<Vec<u8>> = fs
        .read_dir(&root)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != b"." && n != b"..")
        .collect();
    listed.sort();
    for name in listed {
        let inode = fs.lookup(&root, &name).unwrap();
        let mut xattrs = fs.xattrs(&inode).unwrap();
        xattrs.sort();
        for (key, value) in xattrs {
            let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
            ours.push_str(&format!(
                "{} {} {hex}\n",
                String::from_utf8_lossy(&name),
                String::from_utf8_lossy(&key)
            ));
        }
    }
    assert_eq!(
        ours, kernel,
        "this crate and the kernel read different xattrs"
    );
}

/// A compressed inode whose xattrs end 4 bytes past an 8-byte boundary,
/// in each index format: the map header belongs at the next boundary,
/// where `fsck.erofs` and the kernel look for it, and every file must
/// decompress to what went in.
#[test]
fn a_compression_map_after_xattrs_ending_off_8_bytes_is_where_fsck_and_the_kernel_read_it() {
    use fs_erofs::xattr::ns;
    use fs_erofs_test_support::{guest_kernel_read, sha256_hex};
    let data: Vec<u8> = (0..20_000u32)
        .flat_map(|i| format!("{} ", i % 300).into_bytes())
        .take(20_000)
        .collect();
    let node = |index_format, ztailpacking| {
        mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
            mode: mkfs::DEFAULT_FILE_MODE,
            data: data.clone(),
            algo: mkfs::CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: mkfs::NodeMeta::default(),
            xattrs: vec![mkfs::XattrSpec::new(ns::USER, b"a".to_vec(), b"b".to_vec())],
            index_format,
            ztailpacking,
            target_pcluster_blocks: 1,
        })
    };
    let img = mkfs::build_image(
        dir(vec![
            ("legacy", node(mkfs::CompressedIndexFormat::Legacy, false)),
            (
                "legacy-tail",
                node(mkfs::CompressedIndexFormat::Legacy, true),
            ),
            (
                "compact",
                node(mkfs::CompressedIndexFormat::Compacted2B, false),
            ),
            (
                "compact-tail",
                node(mkfs::CompressedIndexFormat::Compacted2B, true),
            ),
        ]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let out = guest_kernel_read(path.to_str().unwrap(), r#"cd "$MNT" && sha256sum -- *"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let kernel = String::from_utf8(out.stdout).unwrap();
    let want = sha256_hex(&data);
    for name in ["legacy", "legacy-tail", "compact", "compact-tail"] {
        assert!(
            kernel.lines().any(|l| l == format!("{want}  {name}")),
            "the kernel read {name} as something else:\n{kernel}"
        );
    }
}

/// Directories, symlinks and file tails inline in the metadata area:
/// `fsck.erofs` accepts them, and the kernel lists every directory, reads
/// every link and hashes every file as they went in.
#[test]
fn inline_directory_symlink_and_file_tails_are_read_by_fsck_and_the_kernel() {
    use fs_erofs_test_support::{guest_kernel_read, sha256_hex};
    let names: Vec<String> = (0..600).map(|i| format!("entry-number-{i:04}")).collect();
    let tail: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let tree = dir(vec![
        (
            "big",
            dir(names
                .iter()
                .map(|n| (n.as_str(), file(n.as_bytes())))
                .collect()),
        ),
        ("small", dir(vec![("a", file(b"a"))])),
        ("empty", dir(vec![])),
        (
            "link",
            mkfs::Node::Symlink {
                mode: mkfs::DEFAULT_SYMLINK_MODE,
                target: "/system/bin/toybox".to_string(),
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
        ("tail", file(&tail)),
    ]);
    let img = mkfs::build_image(tree, 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    let out = guest_kernel_read(
        path.to_str().unwrap(),
        r#"cd "$MNT" && ls big | wc -l && ls small empty && readlink link && sha256sum tail"#,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let kernel = String::from_utf8(out.stdout).unwrap();
    assert!(kernel.lines().any(|l| l.trim() == "600"), "{kernel}");
    assert!(kernel.contains("/system/bin/toybox"), "{kernel}");
    assert!(
        kernel.contains(&format!("{}  tail", sha256_hex(&tail))),
        "{kernel}"
    );
}
