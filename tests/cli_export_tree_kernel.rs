//! Shared recursive export versus the mounted kernel and erofs-utils extractor.
mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{
    assert_fsck_clean, guest_kernel_read, guest_kernel_report, mkfs_from_guest_tree, oracle,
    sha256_hex, ScratchDir,
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
        assert_eq!(json_field(&report, "skipped"), "1", "{report}");
        assert_eq!(json_field(&report, "refused"), "3", "{report}");
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
            let entry = item(&report, &format!("/tree/{path}"));
            let status = if path == "pipe" { "skipped" } else { "refused" };
            assert!(
                entry.contains(&format!("\"status\": \"{status}\"")),
                "{entry}"
            );
            if path != "pipe" {
                assert!(entry.contains("--preserve=links"), "{entry}");
            }
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

#[test]
fn recursive_export_preserves_kernel_links_and_available_times_for_both_inode_versions() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    const STAGE: &str = r#"
mkdir -p tree/sub
printf 'linked content\n' > tree/file
ln tree/file tree/hard
ln tree/file tree/sub/hard2
python3 - <<'PY'
import os
os.symlink(b'file', b'tree/short')
os.symlink(b'long/' * 40 + b'absent', b'tree/long')
os.symlink(b'../absent-\xff\n', b'tree/raw')
os.symlink(b'../..', b'tree/sub/up')
os.utime('tree/file', ns=(981173106123456789, 1234567890987654321))
os.utime('tree/sub', ns=(1000000000500000000, 1100000000250000000))
os.utime('tree', ns=(1000000000123456789, 1200000000123456789))
PY
"#;
    const REPORT: &str = r#"
cd "$MNT"
python3 - <<'PY'
import os
for name in ['file', 'hard', 'sub/hard2', 'sub', '.']:
    s = os.lstat('tree/' + name)
    print('M', name, s.st_ino, s.st_nlink, s.st_mtime_ns)
for name in ['short', 'long', 'raw', 'sub/up']:
    print('L', name, os.readlink(b'tree/' + name.encode()).hex())
PY
"#;

    for layout in ["compact", "extended"] {
        let dir = ScratchDir::new(&format!("export-preserve-{layout}"));
        let image = dir.join("source.img");
        let force = format!("-Eforce-inode-{layout}");
        let built = mkfs_from_guest_tree(&image, STAGE, &["-b4096", &force]);
        assert!(built.status.success(), "mkfs.erofs: {}", stderr(&built));
        assert_fsck_clean(image.to_str().unwrap(), "preserved export source");
        let before = std::fs::read(&image).unwrap();
        let kernel = guest_kernel_read(image.to_str().unwrap(), REPORT);
        assert!(
            kernel.status.success(),
            "{}{}",
            stdout(&kernel),
            stderr(&kernel)
        );
        let kernel = stdout(&kernel);
        let rows: Vec<Vec<&str>> = kernel
            .lines()
            .map(|line| line.split(' ').collect())
            .collect();
        assert_eq!(rows.len(), 9, "{kernel}");
        let reference = dir.join("reference");
        let extracted = oracle("fsck.erofs")
            .arg(format!("--extract={}", reference.display()))
            .arg(&image)
            .output();
        assert!(
            extracted.status.success(),
            "fsck.erofs: {}",
            stderr(&extracted)
        );

        let plain = dir.join("plain");
        let out = tool("fs.erofs")
            .arg(&image)
            .args(["export", "--recursive", "/tree"])
            .arg(&plain)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        let report = stdout(&out);
        for name in ["short", "long", "raw", "sub/up"] {
            let entry = item(&report, &format!("/tree/{name}"));
            assert!(entry.contains("\"status\": \"refused\""), "{entry}");
            assert!(entry.contains("--preserve=links"), "{entry}");
            assert!(std::fs::symlink_metadata(plain.join(name)).is_err());
        }
        assert_ne!(
            std::fs::metadata(plain.join("file")).unwrap().ino(),
            std::fs::metadata(plain.join("hard")).unwrap().ino()
        );

        let dest = dir.join("preserved");
        let out = tool("fs.erofs")
            .arg(&image)
            .args(["export", "--recursive", "--preserve=links,times", "/tree"])
            .arg(&dest)
            .output()
            .unwrap();
        let report = stdout(&out);
        assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));
        assert_eq!(json_field(&report, "completed"), "9", "{report}");
        assert_eq!(json_field(&report, "failed"), "0", "{report}");
        let linked = std::fs::metadata(dest.join("file")).unwrap();
        let first = &rows[0];
        assert_eq!(first[0], "M");
        assert_eq!(first[1], "file");
        assert_eq!(first[3], "3", "the kernel must see three hard links");
        for row in &rows {
            let name = row[1];
            let local = dest.join(name);
            if row[0] == "L" {
                let target = std::fs::read_link(&local).unwrap();
                let raw = target.as_os_str().as_bytes();
                let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(hex, row[2], "{layout} {name}: kernel readlink bytes");
                assert_eq!(
                    target,
                    std::fs::read_link(reference.join("tree").join(name)).unwrap()
                );
                let entry = item(&report, &format!("/tree/{name}"));
                assert!(
                    entry.contains("symbolic link times not preserved"),
                    "{entry}"
                );
                if name == "raw" || name == "sub/up" {
                    assert!(entry.contains("target escapes DEST"), "{entry}");
                }
                continue;
            }
            assert_eq!(row[0], "M");
            let host = std::fs::metadata(&local).unwrap();
            let expected = row[4].parse::<i128>().unwrap();
            assert_eq!(
                i128::from(host.mtime()) * 1_000_000_000 + i128::from(host.mtime_nsec()),
                expected,
                "{layout} {name}: modification time versus kernel"
            );
            let source = if name == "." {
                "/tree".into()
            } else {
                format!("/tree/{name}")
            };
            let stat = stdout(&ok(tool("fs.erofs").arg(&image).arg("stat").arg(&source)));
            assert_eq!(json_field(&stat, "inode"), row[2]);
            assert_eq!(json_field(&stat, "layout"), layout);
            assert_eq!(json_field(&stat, "atime"), "null");
            assert_eq!(json_field(&stat, "atime_nsec"), "null");
            let entry = item(&report, &source);
            assert!(entry.contains("one source time unavailable"), "{entry}");
            if host.is_file() {
                assert_eq!(row[2], first[2], "kernel hard-link identity");
                assert_eq!(
                    host.ino(),
                    linked.ino(),
                    "{name}: exported hard-link identity"
                );
                assert_eq!(host.nlink().to_string(), row[3]);
                assert_eq!(
                    std::fs::read(&local).unwrap(),
                    std::fs::read(reference.join("tree").join(name)).unwrap()
                );
            }
            if layout == "extended" && name == "file" {
                assert_eq!(
                    expected, 1_234_567_890_987_654_321,
                    "fixture retains nanoseconds"
                );
            }
        }
        assert_eq!(
            std::fs::read(&image).unwrap(),
            before,
            "export changed its source"
        );
    }
}
