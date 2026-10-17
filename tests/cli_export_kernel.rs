//! Bounded exports from reference images, checked against Linux and fsck.erofs.

mod cli_support;

use cli_support::*;
use fs_core::FileDevice;
use fs_erofs::{DataLayout, Filesystem, Inode};
use fs_erofs_test_support::{
    assert_fsck_clean, guest_kernel_report, mkfs_from_guest_tree, oracle, sha256_hex, ScratchDir,
};
use std::path::Path;
use std::sync::Arc;

const STAGE: &str =
    "touch empty\npython3 - <<'PY'\nwith open('big', 'wb') as f:\n    f.write(bytes(range(256)) * 12288 + b'!')\nPY\nmkdir dir\nln -s big link\nmkfifo pipe\n";

fn image(dir: &ScratchDir, compressed: bool) -> std::path::PathBuf {
    let image = dir.join("source.img");
    let args = if compressed {
        vec!["-b4096", "-zlz4"]
    } else {
        vec!["-b4096"]
    };
    let built = mkfs_from_guest_tree(&image, STAGE, &args);
    assert!(built.status.success(), "mkfs.erofs: {}", stderr(&built));
    assert_fsck_clean(image.to_str().unwrap(), "export source");
    image
}

#[test]
fn export_matches_kernel_and_reference_digests_with_shared_publication_rules() {
    for compressed in [false, true] {
        let dir = ScratchDir::new("cli-export");
        let image = image(&dir, compressed);
        let fs = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        let layout = fs.lookup_path("/big").unwrap().format.layout;
        assert_eq!(
            matches!(
                layout,
                DataLayout::Compression | DataLayout::CompressionLegacy
            ),
            compressed,
            "the fixture must exercise the requested data layout"
        );
        let before = std::fs::read(&image).unwrap();
        let kernel = guest_kernel_report(image.to_str().unwrap(), "export reference");
        let extracted = dir.join("extracted");
        let reference = oracle("fsck.erofs")
            .arg(format!("--extract={}", extracted.display()))
            .arg(&image)
            .output();
        assert!(
            reference.status.success(),
            "fsck.erofs: {}",
            stderr(&reference)
        );
        for (name, size) in [("empty", 0), ("big", 3_145_729)] {
            let want = kernel
                .get(&("sha256".to_string(), name.to_string()))
                .expect("kernel digested the source file");
            assert_eq!(
                &sha256_hex(&std::fs::read(extracted.join(name)).unwrap()),
                want
            );
            let path = format!("/{name}");
            let dest = dir.join(name);
            let report = stdout(&ok(tool("fs.erofs")
                .arg(&image)
                .args(["export", &path])
                .arg(&dest)));
            assert_eq!(json_field(&report, "bytes"), size.to_string());
            assert_eq!(json_field(&report, "sha256"), *want);
            assert_eq!(json_field(&report, "replaced"), "false");
            let bytes = std::fs::read(&dest).unwrap();
            assert_eq!(bytes.len(), size);
            assert_eq!(&sha256_hex(&bytes), want);
            let raw = ok(tool("fs.erofs").arg(&image).args(["export", &path, "-"]));
            assert_eq!(raw.stdout, bytes);
            assert!(raw.stderr.is_empty());
            std::fs::write(&dest, b"keep me").unwrap();
            let refused = tool("fs.erofs")
                .arg(&image)
                .args(["export", &path])
                .arg(&dest)
                .output()
                .unwrap();
            assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
            assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
            let replaced = stdout(&ok(tool("fs.erofs")
                .arg(&image)
                .args(["export", "--replace", &path])
                .arg(&dest)));
            assert_eq!(json_field(&replaced, "replaced"), "true");
            assert_eq!(&sha256_hex(&std::fs::read(&dest).unwrap()), want);
        }
        for path in ["/dir", "/link", "/pipe", "/missing"] {
            let dest = dir.join("refused");
            let out = tool("fs.erofs")
                .arg(&image)
                .args(["export", path])
                .arg(&dest)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
            assert!(out.stdout.is_empty());
            assert!(!dest.exists());
        }
        let usage = tool("fs.erofs")
            .arg(&image)
            .args(["export", "--replace", "/big", "-"])
            .output()
            .unwrap();
        assert_eq!(usage.status.code(), Some(2));
        assert!(usage.stdout.is_empty());
        let help = stdout(&ok(tool("fs.erofs").arg(&image).args(["export", "--help"])));
        assert!(help.contains("Examples:"), "{help}");
        assert!(help.contains("fs.erofs rootfs.erofs export"), "{help}");
        let embedded = dir.join("embedded");
        let mut bytes = vec![0; 4096];
        bytes.extend_from_slice(&before);
        std::fs::write(&embedded, bytes).unwrap();
        let out = ok(tool("fs.erofs")
            .arg(&embedded)
            .args(["--offset", "4096", "export", "/big", "-"]));
        assert_eq!(
            &sha256_hex(&out.stdout),
            kernel.get(&("sha256".into(), "big".into())).unwrap()
        );
        assert_eq!(
            std::fs::read(&image).unwrap(),
            before,
            "export changed its source"
        );
    }
}

#[test]
fn unreadable_export_never_publishes_or_replaces_and_cleans_staging() {
    let dir = ScratchDir::new("cli-export-damaged");
    let image = image(&dir, false);
    let fs = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let inode = fs.lookup_path("/big").unwrap();
    assert!(matches!(
        inode.format.layout,
        DataLayout::FlatPlain | DataLayout::FlatInline
    ));
    let at = Inode::iloc(fs.superblock(), inode.nid) as usize + 16;
    let mut bytes = std::fs::read(&image).unwrap();
    // Keep metadata intact, but make the file's data run beyond the image.
    // One full shared-transfer chunk remains readable before the failure.
    let block = u32::try_from(bytes.len() / 4096 - 257).unwrap();
    bytes[at..at + 4].copy_from_slice(&block.to_le_bytes());
    if fs.superblock().feature_compat & 1 != 0 {
        bytes[1028..1032].fill(0);
        let end = 1024 + fs.superblock().checksum_span_len();
        let checksum = crc32c::crc32c(&bytes[1024..end]) ^ u32::MAX;
        bytes[1028..1032].copy_from_slice(&checksum.to_le_bytes());
    }
    std::fs::write(&image, bytes).unwrap();
    let damaged = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let inode = damaged.lookup_path("/big").unwrap();
    let mut chunk = vec![0; 1 << 20];
    damaged.read_file(&inode, 0, &mut chunk).unwrap();
    assert!(damaged.read_file(&inode, 1 << 20, &mut chunk).is_err());
    let dest = dir.join("destination");
    let out = tool("fs.erofs")
        .arg(&image)
        .args(["export", "/big"])
        .arg(&dest)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(!dest.exists());
    std::fs::write(&dest, b"original destination").unwrap();
    let out = tool("fs.erofs")
        .arg(&image)
        .args(["export", "--replace", "/big"])
        .arg(&dest)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), b"original destination");
    assert_no_staging(&dest);
}

fn assert_no_staging(dest: &Path) {
    let stem = format!(".{}.", dest.file_name().unwrap().to_string_lossy());
    for entry in std::fs::read_dir(dest.parent().unwrap()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(&stem), "staging file remained: {name}");
    }
}
