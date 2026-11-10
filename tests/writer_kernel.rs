//! The builder's images as the real kernel reads them: each test here has
//! `fsck.erofs` accept an image this crate wrote, then mounts it in the
//! harness VM.
//!
//! In a file of their own because `scripts/test-targets.sh` sorts a whole
//! file by what it calls: one kernel call in `oracle_writer.rs` would take
//! all of it out of the oracle tier.

mod common;

use common::{dir, open_image_path};
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
