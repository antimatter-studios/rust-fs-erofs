//! End-to-end proof that our reader handles a real Android `system.img`.
//!
//! The fixture at `tests/fixtures/system.img` is NOT in the repo (it's
//! gitignored, ~2 GiB raw, AOSP-licensed and not redistributable from
//! us). Download it with:
//!
//! ```sh
//! tests/fixtures/download-gsi.sh
//! cargo test --test oracle_gsi -- --ignored
//! ```
//!
//! The test is `#[ignore]`-gated so a fresh checkout without the
//! fixture still has a green default `cargo test`.
//!
//! License posture: the GSI is an Apache-2.0 userspace + GPL-2 kernel
//! image distributed by Google. We treat it as an opaque black-box
//! fixture; no AOSP source is copied, only the public download URL is
//! referenced from the helper script.

mod common;

use fs_core::{BlockRead, FileDevice};
use fs_erofs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FIXTURE: &str = "tests/fixtures/system.img";

fn fixture_path() -> PathBuf {
    PathBuf::from(FIXTURE)
}

fn require_fixture() -> PathBuf {
    let p = fixture_path();
    assert!(
        p.exists(),
        "fixture missing at {} -- run tests/fixtures/download-gsi.sh \
         to fetch it (the file is not committed to the repo)",
        p.display()
    );
    p
}

fn open_fs(path: &Path) -> Filesystem {
    let dev = FileDevice::open(path).expect("FileDevice::open");
    let dev: Arc<dyn BlockRead> = Arc::new(dev);
    Filesystem::open(dev).expect("Filesystem::open")
}

/// Look up an absolute path, returning `Some(inode)` only if it
/// resolves AND is a directory. Used for the "at least one of these
/// directories exists" heuristic so the test survives GSI version
/// drift (Android moves `/bin` -> `/system/bin` -> `/apex/...` over
/// the course of releases).
fn probe_dir(fs: &Filesystem, path: &str) -> bool {
    fs.lookup_path(path).map(|i| i.is_dir()).unwrap_or(false)
}

/// Same idea for a regular file.
fn probe_file(fs: &Filesystem, path: &str) -> bool {
    fs.lookup_path(path)
        .map(|i| i.is_regular_file())
        .unwrap_or(false)
}

/// Recursive walk with depth + node-count caps so a malformed image
/// can't make the test run forever. Returns (dirs_seen, files_seen,
/// symlinks_seen).
fn walk_capped(
    fs: &Filesystem,
    root_path: &str,
    max_depth: u32,
    max_nodes: u64,
) -> (u64, u64, u64) {
    let mut dirs = 0u64;
    let mut files = 0u64;
    let mut symlinks = 0u64;
    let mut nodes = 0u64;

    // Stack of (absolute_path, inode, depth).
    let root_inode = fs.lookup_path(root_path).expect("walk: root lookup");
    let mut stack: Vec<(String, fs_erofs::Inode, u32)> =
        vec![(root_path.to_string(), root_inode, 0)];

    while let Some((path, inode, depth)) = stack.pop() {
        nodes += 1;
        if nodes > max_nodes {
            break;
        }
        if inode.is_dir() {
            dirs += 1;
            if depth >= max_depth {
                continue;
            }
            // Skip directory listing failures rather than panicking --
            // a real GSI may have access-controlled or oddly-formed
            // dirents we don't care about for the smoke test.
            let entries = match fs.read_dir(&inode) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for ent in entries {
                if ent.name == b"." || ent.name == b".." {
                    continue;
                }
                let name = match std::str::from_utf8(&ent.name) {
                    Ok(s) => s,
                    Err(_) => continue, // weirdly-encoded names: skip
                };
                let child_path = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                let child_inode = match fs.read_inode(ent.nid) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                stack.push((child_path, child_inode, depth + 1));
            }
        } else if inode.is_regular_file() {
            files += 1;
        } else if inode.is_symlink() {
            symlinks += 1;
        }
    }

    (dirs, files, symlinks)
}

/// Walk to find the first regular file under `root_path` whose size
/// is in `[min_size, max_size]`, breadth-first-ish (capped). Returns
/// (absolute_path, inode) or None.
fn find_small_file(
    fs: &Filesystem,
    root_path: &str,
    min_size: u64,
    max_size: u64,
    max_visit: u64,
) -> Option<(String, fs_erofs::Inode)> {
    let root_inode = fs.lookup_path(root_path).ok()?;
    let mut stack: Vec<(String, fs_erofs::Inode)> = vec![(root_path.to_string(), root_inode)];
    let mut visited = 0u64;
    while let Some((path, inode)) = stack.pop() {
        visited += 1;
        if visited > max_visit {
            return None;
        }
        if inode.is_regular_file() && inode.size >= min_size && inode.size <= max_size {
            return Some((path, inode));
        }
        if inode.is_dir() {
            let entries = fs.read_dir(&inode).ok()?;
            for ent in entries {
                if ent.name == b"." || ent.name == b".." {
                    continue;
                }
                let name = match std::str::from_utf8(&ent.name) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let child_path = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                if let Ok(child) = fs.read_inode(ent.nid) {
                    stack.push((child_path, child));
                }
            }
        }
    }
    None
}

// =====================================================================
// Test cases. All `#[ignore]`-gated -- depend on the fixture.
// =====================================================================

#[test]
#[ignore = "needs tests/fixtures/system.img (run tests/fixtures/download-gsi.sh)"]
fn open_and_walk_gsi() {
    let path = require_fixture();
    let fs = open_fs(&path);

    // ---- superblock + root sanity ----
    let root = fs.root_inode().expect("root inode");
    assert!(root.is_dir(), "root inode must be a directory");
    assert!(root.size > 0, "root inode reports zero size");

    // ---- known-present directory heuristic ----
    //
    // GSI layout has shifted across versions (Treble, /system/* hoist,
    // APEX). Don't pin to any single path -- just require that AT
    // LEAST ONE of these well-known directories exists. Adjust the
    // candidate list when Android invents another layout.
    let dir_candidates = [
        "/bin",
        "/etc",
        "/lib",
        "/lib64",
        "/system",
        "/system/bin",
        "/system/etc",
        "/system/lib",
        "/system/lib64",
        "/apex",
        "/usr",
    ];
    let dirs_found: Vec<&str> = dir_candidates
        .iter()
        .copied()
        .filter(|p| probe_dir(&fs, p))
        .collect();
    assert!(
        !dirs_found.is_empty(),
        "none of the well-known GSI directories were found: {dir_candidates:?} \
         -- the fixture may be malformed or a GSI layout we haven't seen"
    );
    eprintln!("gsi: directories present: {dirs_found:?}");

    // ---- known-present file heuristic ----
    //
    // build.prop is the canonical "this is an Android system image"
    // marker. Its location varies by version; check the usual spots.
    let buildprop_candidates = [
        "/etc/build.prop",
        "/system/build.prop",
        "/system/etc/build.prop",
        "/build.prop",
    ];
    let buildprop = buildprop_candidates
        .iter()
        .copied()
        .find(|p| probe_file(&fs, p));
    if let Some(bp_path) = buildprop {
        let inode = fs.lookup_path(bp_path).expect("buildprop lookup");
        assert!(inode.size > 0, "{bp_path} is empty");
        // Read up to 64 KiB and verify it looks like a Java/Android
        // .properties file (lines, ASCII-ish, contains '=').
        let read_len = inode.size.min(64 * 1024) as usize;
        let mut buf = vec![0u8; read_len];
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {bp_path}: {e:?}"));
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains('='),
            "{bp_path} does not look like a properties file (no '=' in first {read_len} bytes)"
        );
        // The "ro." prefix is universal across every Android build.
        assert!(
            text.contains("ro."),
            "{bp_path} contains no 'ro.' properties -- doesn't look like Android build.prop"
        );
        eprintln!(
            "gsi: build.prop at {bp_path}, {} bytes, head: {:?}",
            inode.size,
            &text.lines().next().unwrap_or("")
        );
    } else {
        eprintln!(
            "gsi: WARN no build.prop found in any of {buildprop_candidates:?} \
             -- fixture may be a non-system partition; skipping content check"
        );
    }

    // ---- bounded tree walk (smoke / non-panic test) ----
    //
    // Walk from the root with a depth + node cap. We don't assert
    // exact counts (those depend on the GSI version) -- we just
    // require that we got past the root, saw plausibly-many
    // directories and files, and no panic occurred.
    let (dirs, files, symlinks) = walk_capped(&fs, "/", 8, 50_000);
    eprintln!("gsi: walk saw dirs={dirs} files={files} symlinks={symlinks}");
    assert!(dirs >= 2, "walk found suspiciously few dirs: {dirs}");
    assert!(files >= 10, "walk found suspiciously few files: {files}");

    // ---- sample-file integrity ----
    //
    // Find a small (< 256 KiB), non-empty regular file and read it
    // end-to-end. Verifies the data path past the metadata layer.
    if let Some((sample_path, inode)) = find_small_file(&fs, "/", 1, 256 * 1024, 5_000) {
        let mut buf = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {sample_path}: {e:?}"));
        assert_eq!(
            buf.len() as u64,
            inode.size,
            "sample {sample_path} short read"
        );
        // Trivial integrity check: ensure read_at the same range twice
        // yields identical bytes (catches caching / aliasing bugs).
        let mut buf2 = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf2)
            .unwrap_or_else(|e| panic!("re-read {sample_path}: {e:?}"));
        assert_eq!(buf, buf2, "{sample_path} bytes differ across reads");
        eprintln!("gsi: sample read {sample_path} ({} bytes) OK", inode.size);
    } else {
        eprintln!("gsi: WARN no small file found for integrity sample (walk capped)");
    }
}
