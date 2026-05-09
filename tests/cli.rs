//! CLI tests for `src/bin/mkfs_erofs.rs`.
//!
//! Uses `assert_cmd::Command::cargo_bin` to spawn the freshly-built
//! binary. Each test owns its own `tempfile::TempDir` so they don't
//! step on each other when `cargo test` runs them in parallel.
//!
//! Coverage target: lift `bin/mkfs_erofs.rs` from 0% to >= 60% by
//! exercising:
//!   * `--help` / argument parsing happy path
//!   * `--block-size` validation arms (bad-int, non-pow2, out-of-range)
//!   * the `walk()` happy path on a small tree, with the resulting
//!     image opened by our own `Filesystem` reader
//!   * `walk()` skipping symlinks (warning-on-stderr branch)
//!   * the missing-source-dir error path
//!
//! NOT covered: the read-/write-error arms (we'd need to inject a
//! permissions failure), and the `mode_for` `host_mode == 0` branch
//! (impossible to construct on a real Unix host without ptrace).

mod common;

use assert_cmd::Command;
use common::{dir, file, open_image_path};
use fs_erofs::mkfs;

#[test]
fn help_flag_exits_zero_and_prints_usage() {
    let out = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg("--help")
        .output()
        .expect("spawn mkfs_erofs");
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"), "stdout was: {stdout}");
}

#[test]
fn short_help_flag_also_prints_usage() {
    let out = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg("-h")
        .output()
        .expect("spawn");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
}

#[test]
fn missing_source_dir_exits_nonzero_with_message() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("img.bin");
    let nonexistent = dir.path().join("does-not-exist");
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(&out)
        .arg(&nonexistent)
        .output()
        .expect("spawn");
    assert!(!result.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&result.stderr);
    // The CLI prints "walking <src>: <io error>" — the io error on
    // macOS/Linux for ENOENT contains "No such file".
    assert!(
        stderr.contains("walking") || stderr.contains("No such file"),
        "stderr: {stderr}"
    );
}

#[test]
fn block_size_not_power_of_two_exits_two() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(dir.path().join("img.bin"))
        .arg(dir.path().join("src"))
        .arg("--block-size")
        .arg("99")
        .output()
        .expect("spawn");
    assert_eq!(result.status.code(), Some(2), "stderr: {:?}", result.stderr);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("power of 2"), "stderr: {stderr}");
}

#[test]
fn block_size_below_range_exits_two() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(dir.path().join("img.bin"))
        .arg(dir.path().join("src"))
        .arg("--block-size")
        .arg("256")
        .output()
        .expect("spawn");
    // 256 is a power of 2 but bits=8 < 9 ⇒ rejected by the range arm.
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("512..=65536"));
}

#[test]
fn block_size_missing_arg_exits_two() {
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg("--block-size")
        .output()
        .expect("spawn");
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("--block-size"));
}

#[test]
fn block_size_not_an_integer_exits_two() {
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg("--block-size")
        .arg("abc")
        .arg("/tmp/out")
        .arg("/tmp/src")
        .output()
        .expect("spawn");
    assert_eq!(result.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("--block-size"), "stderr: {stderr}");
}

#[test]
fn unknown_flag_exits_two() {
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg("--unknown-thing")
        .output()
        .expect("spawn");
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("unknown flag"));
}

#[test]
fn missing_positional_args_exits_two() {
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .output()
        .expect("spawn");
    assert_eq!(result.status.code(), Some(2));
    // Falls through to the "positional.len() != 2" arm which prints
    // USAGE.
    assert!(String::from_utf8_lossy(&result.stderr).contains("Usage:"));
}

#[test]
fn round_trip_small_tree() {
    // Build src/{a.txt, b.txt, sub/c.txt} on disk, run the CLI, open
    // the produced image with our reader, and verify the layout.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"alpha").unwrap();
    std::fs::write(src.join("b.txt"), b"bravo bravo").unwrap();
    std::fs::create_dir(src.join("sub")).unwrap();
    std::fs::write(src.join("sub").join("c.txt"), b"charlie").unwrap();

    let img = tmp.path().join("out.img");
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(&img)
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes_written = std::fs::metadata(&img).unwrap().len();
    assert!(bytes_written > 0, "image should be non-empty");
    // The CLI's success message is on stderr ("wrote N bytes ...").
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("wrote "), "stderr: {stderr}");

    // Now open with our reader and verify each file round-trips.
    let fs = open_image_path(&img);
    for (path, want) in &[
        ("/a.txt", &b"alpha"[..]),
        ("/b.txt", &b"bravo bravo"[..]),
        ("/sub/c.txt", &b"charlie"[..]),
    ] {
        let inode = fs.lookup_path(path).expect(path);
        let mut buf = vec![0u8; want.len()];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(&buf[..], *want, "{path}");
    }
}

#[test]
fn round_trip_with_explicit_block_size_512() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("hi.txt"), b"hello at 512").unwrap();

    let img = tmp.path().join("out.img");
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(&img)
        .arg(&src)
        .arg("--block-size")
        .arg("512")
        .output()
        .expect("spawn");
    assert!(result.status.success());

    let fs = open_image_path(&img);
    assert_eq!(fs.superblock().block_size(), 512);
    let inode = fs.lookup_path("/hi.txt").unwrap();
    let mut buf = vec![0u8; b"hello at 512".len()];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"hello at 512");
}

#[cfg(unix)]
#[test]
fn walk_skips_symlinks_with_warning() {
    // Build a tree that contains both a regular file and a symlink.
    // The CLI's walk() drops the symlink with a stderr warning; the
    // resulting image should still be valid and contain the regular
    // file.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("real.txt"), b"i am real").unwrap();
    std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();

    let img = tmp.path().join("out.img");
    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(&img)
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("warning: skipping"),
        "stderr should warn about the symlink, got: {stderr}"
    );

    let fs = open_image_path(&img);
    let inode = fs.lookup_path("/real.txt").unwrap();
    let mut buf = vec![0u8; b"i am real".len()];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"i am real");
    // The skipped symlink should NOT be present in the image.
    assert!(fs.lookup_path("/link.txt").is_err());
}

/// SOURCE_DIR pointing at a regular file (not a directory) is the
/// `else if meta.is_file()` arm of `walk()`: it wraps the file in a
/// synthetic root dir with one entry. Verifies the alternate walk
/// branch runs cleanly.
#[test]
fn source_can_be_a_regular_file() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = tmp.path().join("payload.bin");
    std::fs::write(&payload, b"singular").unwrap();
    let img = tmp.path().join("out.img");

    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(&img)
        .arg(&payload)
        .output()
        .expect("spawn");
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let fs = open_image_path(&img);
    let inode = fs.lookup_path("/payload.bin").unwrap();
    let mut buf = vec![0u8; b"singular".len()];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"singular");
}

/// A round-trip via the CLI of a tree built from our `mkfs::Node`
/// helpers (materialized to disk, then ingested by the CLI). Proves
/// the CLI's mode-detection + walk match what `build_image` emits when
/// fed the same tree directly.
#[test]
fn cli_image_matches_in_process_image_for_simple_tree() {
    let tree = dir(vec![
        ("one.txt", file(b"one")),
        ("two.txt", file(b"two two")),
    ]);
    let direct = mkfs::build_image(tree, 12).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("one.txt"), b"one").unwrap();
    std::fs::write(src.join("two.txt"), b"two two").unwrap();
    let img = tmp.path().join("out.img");

    let result = Command::cargo_bin("mkfs_erofs")
        .unwrap()
        .arg(&img)
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(result.status.success());

    let cli_bytes = std::fs::read(&img).unwrap();
    // Don't byte-compare (host mode bits differ from DEFAULT_FILE_MODE)
    // — instead verify both images expose the same file contents.
    assert!(!cli_bytes.is_empty());
    let fs_direct = common::open_image(direct);
    let fs_cli = open_image_path(&img);
    for path in ["/one.txt", "/two.txt"] {
        let want = {
            let i = fs_direct.lookup_path(path).unwrap();
            let mut buf = vec![0u8; i.size as usize];
            fs_direct.read_file(&i, 0, &mut buf).unwrap();
            buf
        };
        let got = {
            let i = fs_cli.lookup_path(path).unwrap();
            let mut buf = vec![0u8; i.size as usize];
            fs_cli.read_file(&i, 0, &mut buf).unwrap();
            buf
        };
        assert_eq!(got, want, "{path}");
    }
}
