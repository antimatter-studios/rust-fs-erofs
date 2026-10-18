//! Shared recursive export versus the mounted kernel and erofs-utils extractor.
mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{
    assert_fsck_clean, guest_kernel_report, mkfs_from_guest_tree, oracle, sha256_hex, ScratchDir,
};

const DEPTH: usize = 33;

/// The report's item after its path key, up to the next item's path key.
fn item<'a>(report: &'a str, path: &str) -> &'a str {
    let needle = format!("\"path\": \"{path}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {path} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find("\"path\": ").unwrap_or(rest.len())]
}

#[test]
fn recursive_export_matches_kernel_and_reference_digests_and_reports_each_item() {
    for compressed in [false, true] {
        let dir = ScratchDir::new("cli-export-tree");
        let image = dir.join("source.img");
        let stage = format!(
            "mkdir tree\ncd tree\n: > empty\n\
             python3 - <<'PY'\n\
             with open('big', 'wb') as f:\n    f.write(bytes(range(256)) * 12288 + b'!')\nPY\n\
             p=.\nfor i in $(seq 1 {DEPTH}); do\n\
             p=\"$p/d$i\"\nmkdir \"$p\"\nprintf 'file %s' \"$i\" > \"$p/f$i\"\ndone\n\
             ln -s big link\nln -s absent dangling\nln -s ../.. d1/up\nmkfifo pipe\n"
        );
        let args = if compressed {
            vec!["-b4096", "-zlz4"]
        } else {
            vec!["-b4096"]
        };
        let built = mkfs_from_guest_tree(&image, &stage, &args);
        assert!(built.status.success(), "mkfs.erofs: {}", stderr(&built));
        assert_fsck_clean(image.to_str().unwrap(), "recursive export source");
        let before = std::fs::read(&image).unwrap();
        let kernel = guest_kernel_report(image.to_str().unwrap(), "recursive export reference");
        let sums: Vec<_> = kernel
            .iter()
            .filter(|((kind, _), _)| kind == "sha256")
            .collect();
        assert_eq!(sums.len(), DEPTH + 2, "kernel digested every regular file");
        assert_eq!(
            kernel.get(&("size".into(), "tree/big".into())).unwrap(),
            "3145729"
        );
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

        let dest = dir.join("exported");
        let plain = tool("fs.erofs")
            .arg(&image)
            .args(["export", "/tree"])
            .arg(&dest)
            .output()
            .unwrap();
        assert_eq!(plain.status.code(), Some(1), "{}", stderr(&plain));
        assert!(
            !dest.exists(),
            "without --recursive directories remain refused"
        );

        let out = tool("fs.erofs")
            .arg(&image)
            .args(["export", "--recursive", "/tree"])
            .arg(&dest)
            .output()
            .unwrap();
        let report = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
        assert_eq!(
            json_field(&report, "completed"),
            (DEPTH * 2 + 3).to_string()
        );
        assert_eq!(json_field(&report, "skipped"), "4", "{report}");
        assert_eq!(json_field(&report, "failed"), "0", "{report}");
        for ((_, path), want) in sums {
            let bytes = std::fs::read(dest.join(path.strip_prefix("tree/").unwrap())).unwrap();
            assert_eq!(&sha256_hex(&bytes), want, "{path}: exported versus kernel");
            assert_eq!(
                &sha256_hex(&std::fs::read(extracted.join(path)).unwrap()),
                want,
                "{path}: reference extractor versus kernel"
            );
            let entry = item(&report, &format!("/{path}"));
            assert!(entry.contains("\"status\": \"completed\""), "{entry}");
            assert!(entry.contains(want.as_str()), "{entry}");
        }
        let mut deepest = dest.clone();
        for i in 1..=DEPTH {
            deepest.push(format!("d{i}"));
            assert!(deepest.is_dir(), "directory {i} was exported");
            assert!(item(
                &report,
                &format!("/tree/{}", deepest.strip_prefix(&dest).unwrap().display())
            )
            .contains("\"status\": \"completed\""));
        }
        for path in ["link", "dangling", "d1/up", "pipe"] {
            assert!(item(&report, &format!("/tree/{path}")).contains("\"status\": \"skipped\""));
            assert!(std::fs::symlink_metadata(dest.join(path)).is_err());
        }

        let kept = dest.join("big");
        std::fs::write(&kept, b"keep me").unwrap();
        let again = tool("fs.erofs")
            .arg(&image)
            .args(["export", "--recursive", "/tree"])
            .arg(&dest)
            .output()
            .unwrap();
        assert_eq!(again.status.code(), Some(1), "{}", stderr(&again));
        assert!(item(&stdout(&again), "/tree/big").contains("\"status\": \"refused\""));
        assert_eq!(std::fs::read(&kept).unwrap(), b"keep me");
        assert_eq!(
            std::fs::read(&image).unwrap(),
            before,
            "export changed its source"
        );
    }
}
