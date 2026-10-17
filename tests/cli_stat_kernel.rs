//! Every stored stat field versus GNU stat on mounted reference images.
//! Absent access, change and birth times remain null, even though Linux
//! synthesizes access and change times from the single recorded time.
mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{guest_kernel_stat_report, mkfs_from_guest_tree, ScratchDir};
use std::os::unix::ffi::OsStringExt;

const STAGE: &str = r#"
mkdir dir
chmod 1777 dir
head -c 5000 /dev/urandom > file
chown 1234:5678 file
chmod 0640 file
touch -m -d '2001-02-03 04:05:06.123456789' file
ln file hard
: > empty
printf x > one
head -c 4096 /dev/zero > block
head -c 200000 /dev/zero > compressed
ln -s file link
mkfifo fifo
mknod chr c 1 3
mknod blk b 2748 74565
setfattr -n user.first -v one file
setfattr -n user.second -v two file
printf byte > $'nonutf-\xff'
"#;

fn nanos(human: &str) -> String {
    let fraction = human.split_once('.').expect("stat fraction").1;
    let fraction = fraction.split(' ').next().unwrap();
    assert_eq!(fraction.len(), 9, "{human}");
    fraction.parse::<u32>().unwrap().to_string()
}

fn check_layout(layout: &str) {
    let dir = ScratchDir::new(&format!("stat-{layout}"));
    // Raw, inline, compressed and sparse chunk layouts all have to use
    // the kernel's allocation accounting, including zero-length files.
    for extra in ["", "-Enoinline_data", "-zlz4", "--chunksize=4096"] {
        let image = dir.join("stat.img");
        let force = format!("-Eforce-inode-{layout}");
        let mut args = vec!["-b4096", force.as_str()];
        if !extra.is_empty() {
            args.push(extra);
        }
        let built = mkfs_from_guest_tree(&image, STAGE, &args);
        assert!(
            built.status.success(),
            "{}{}",
            stdout(&built),
            stderr(&built)
        );
        let image = image.to_str().unwrap();
        let before = std::fs::read(image).unwrap();
        let report = guest_kernel_stat_report(image);
        let mut lines = report.lines();
        let mut seen = 0;
        while let Some(line) = lines.next() {
            let f: Vec<_> = line.split('|').collect();
            assert_eq!(f.len(), 19, "{line}");
            let mut bytes: Vec<u8> = (0..f[0].len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&f[0][i..i + 2], 16).unwrap())
                .collect();
            assert_eq!(bytes.remove(0), b'.');
            if bytes.is_empty() {
                bytes.push(b'/');
            }
            let path = std::ffi::OsString::from_vec(bytes.clone());
            let ours = stdout(&ok(tool("fs.erofs").args([image, "stat"]).arg(&path)));
            let field = |key| json_field(&ours, key);
            let context = format!("{layout} {extra} {path:?}\n{ours}\n{line}");
            let mode = u32::from_str_radix(f[2], 16).unwrap();
            let kind = match mode & 0o170000 {
                0o100000 => "file",
                0o040000 => "dir",
                0o120000 => "symlink",
                0o020000 => "char",
                0o060000 => "block",
                0o010000 => "fifo",
                other => panic!("unexpected mode {other:o}"),
            };
            for (key, value) in [
                ("inode", f[1]),
                ("uid", f[3]),
                ("gid", f[4]),
                ("nlink", f[5]),
                ("size", f[6]),
                ("mtime", f[11]),
            ] {
                assert_eq!(field(key), value, "{key}: {context}");
            }
            assert_eq!(field("type"), kind, "{context}");
            assert_eq!(field("mode"), format!("{:04o}", mode & 0o7777), "{context}");
            let allocated = f[7].parse::<u64>().unwrap() * f[8].parse::<u64>().unwrap();
            assert_eq!(field("allocated_bytes"), allocated.to_string(), "{context}");
            assert_eq!(field("mtime_nsec"), nanos(f[12]), "{context}");
            for key in [
                "atime",
                "atime_nsec",
                "ctime",
                "ctime_nsec",
                "btime",
                "btime_nsec",
            ] {
                assert_eq!(field(key), "null", "{key}: {context}");
            }
            assert_eq!(f[16], "-", "kernel also has no birth time: {context}");
            if kind == "symlink" {
                assert_eq!(lines.next(), Some("target|file"));
                assert_eq!(field("target"), "file", "{context}");
            }
            if kind == "char" || kind == "block" {
                for (key, raw) in [("major", f[17]), ("minor", f[18])] {
                    assert_eq!(
                        field(key),
                        u32::from_str_radix(raw, 16).unwrap().to_string(),
                        "{context}"
                    );
                }
            }
            let count = lines.next().unwrap().strip_prefix("xattrs|").unwrap();
            assert_eq!(field("xattr_count"), count, "{context}");
            assert_eq!(field("layout"), layout, "{context}");
            if std::str::from_utf8(&bytes).is_err() {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(field("path_hex"), hex, "{context}");
            } else {
                assert_eq!(field("path"), path.to_str().unwrap(), "{context}");
            }
            seen += 1;
        }
        assert_eq!(seen, 13, "every source path, including root: {report}");
        assert_eq!(
            std::fs::read(image).unwrap(),
            before,
            "stat leaves the image unchanged"
        );
    }
}

#[test]
fn stat_matches_the_kernel_on_compact_inodes() {
    check_layout("compact");
}

#[test]
fn stat_matches_the_kernel_on_extended_inodes() {
    check_layout("extended");
}
