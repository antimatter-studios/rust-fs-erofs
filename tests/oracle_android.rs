//! A real Android EROFS image, read inode by inode and compared against
//! what two readers that are not this crate saw in the same bytes.
//!
//! THE IMAGE IS NOBODY'S HERE. `tests/fixtures/android-system_dlkm.img` is
//! the `system_dlkm` partition of Google's Android 16 (API 36) aosp_atd
//! arm64 emulator image, as Google's build wrote it -- not an image this
//! repository built, and not one `mkfs.erofs` made in a temp directory,
//! which is what every other oracle test reads. LZ4-compressed (98 of its
//! 108 inodes), big pclusters, 0padding, and a `security.selinux` label on
//! every inode. Fetch it with:
//!
//! ```sh
//! tests/fixtures/download-android-erofs.sh
//! chore test:android
//! ```
//!
//! THE EXPECTATION IS NOT OURS EITHER. `tests/fixtures/android-system_dlkm.manifest`
//! lists every inode's path, type, permission bits, owner, size, content
//! hash and xattrs as the Linux kernel's EROFS driver read them from a
//! read-only loop mount, and fsck.erofs `--extract` agreed on every path,
//! type, size and hash. So a disagreement here is this crate misreading a
//! real image, not two of our own readers agreeing with each other.
//!
//! IT DOES NOT SKIP, AND IT IS NOT IN `chore test`. It replaced a suite
//! over an Android GSI that could never pass -- every GSI Google publishes
//! ships an ext4 `system.img` (#133) -- and that before that reported ok
//! having opened no image (#54). The image is Google's to distribute, not
//! ours, so it is fetched rather than committed, and this suite is its own
//! tier, `chore test:android`, run nightly by `.github/workflows/android.yml`.
//! Inside the tier a missing fixture FAILS, naming the script that fetches
//! it.

use fs_core::{BlockRead, FileDevice};
use fs_erofs::{Filesystem, Inode};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

const FIXTURE: &str = "tests/fixtures/android-system_dlkm.img";
const MANIFEST: &str = "tests/fixtures/android-system_dlkm.manifest";

/// The fixture, or a failure that says how to fetch it.
#[track_caller]
fn require_fixture() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    assert!(
        p.exists(),
        "{FIXTURE} is missing. Fetch it with tests/fixtures/download-android-erofs.sh \
         (a 712 MB download, of which 7 MB is kept; Google's to distribute, not ours), \
         then run `chore test:android`. Tests never skip on a missing fixture."
    );
    p
}

/// The committed manifest, comment lines dropped, one entry per line.
fn expected() -> Vec<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(MANIFEST);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {MANIFEST}: {e}"));
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One manifest line for `inode` at `path`, in the manifest's format.
/// Every read is unwrapped: an inode this crate cannot read is a failure
/// naming it, never an entry quietly left out.
fn line(fs: &Filesystem, path: &str, inode: &Inode) -> String {
    let (kind, size, hash) = if inode.is_dir() {
        ("d", "-".to_string(), "-".to_string())
    } else if inode.is_symlink() {
        let target = fs
            .read_symlink_target(inode)
            .unwrap_or_else(|e| panic!("{path}: read_symlink_target: {e:?}"));
        ("l", inode.size.to_string(), hex(&Sha256::digest(&target)))
    } else if inode.is_regular_file() {
        let len = usize::try_from(inode.size).expect("file size fits in memory");
        let mut data = vec![0u8; len];
        fs.read_file(inode, 0, &mut data)
            .unwrap_or_else(|e| panic!("{path}: read_file of {len} bytes: {e:?}"));
        ("f", inode.size.to_string(), hex(&Sha256::digest(&data)))
    } else {
        panic!(
            "{path}: mode {:o} is neither a directory, a file nor a symlink, and the \
             manifest holds only those",
            inode.mode
        );
    };
    let mut xattrs: Vec<String> = fs
        .xattrs(inode)
        .unwrap_or_else(|e| panic!("{path}: xattrs: {e:?}"))
        .into_iter()
        .map(|(name, value)| format!("{}={}", String::from_utf8_lossy(&name), hex(&value)))
        .collect();
    xattrs.sort();
    let xattrs = if xattrs.is_empty() {
        "-".to_string()
    } else {
        xattrs.join(",")
    };
    format!(
        "{path}\t{kind}\t{:04o}\t{}:{}\t{size}\t{hash}\t{xattrs}",
        inode.mode & 0o7777,
        inode.uid,
        inode.gid
    )
}

/// Every inode reachable from the root, as manifest lines, sorted.
fn walk(fs: &Filesystem) -> Vec<String> {
    let root = fs.root_inode().expect("root inode");
    let mut out = vec![line(fs, "/", &root)];
    let mut stack = vec![("/".to_string(), root)];
    while let Some((path, dir)) = stack.pop() {
        let entries = fs
            .read_dir(&dir)
            .unwrap_or_else(|e| panic!("{path}: read_dir: {e:?}"));
        for ent in entries {
            if ent.name == b"." || ent.name == b".." {
                continue;
            }
            let name = String::from_utf8(ent.name.clone())
                .unwrap_or_else(|_| panic!("{path}: a name that is not UTF-8: {:?}", ent.name));
            let child_path = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            let child = fs
                .read_inode(ent.nid)
                .unwrap_or_else(|e| panic!("{child_path}: read_inode({}): {e:?}", ent.nid));
            out.push(line(fs, &child_path, &child));
            if child.is_dir() {
                stack.push((child_path, child));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_inode_reads_as_the_kernel_and_fsck_erofs_read_it() {
    let path = require_fixture();
    let dev: Arc<dyn BlockRead> = Arc::new(FileDevice::open(&path).expect("FileDevice::open"));
    let fs = Filesystem::open(dev).expect("Filesystem::open");

    let want = expected();
    // The manifest is the oracle; one that lost its lines would make any
    // walk agree with it.
    assert_eq!(
        want.len(),
        108,
        "{MANIFEST} should list the image's 108 inodes"
    );
    let got = walk(&fs);

    let missing: Vec<&String> = want.iter().filter(|l| !got.contains(l)).collect();
    let extra: Vec<&String> = got.iter().filter(|l| !want.contains(l)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "this crate read {} inodes where the kernel and fsck.erofs read {}.\n\
         expected, not read ({}):\n  {}\nread, not expected ({}):\n  {}",
        got.len(),
        want.len(),
        missing.len(),
        missing
            .iter()
            .take(10)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  "),
        extra.len(),
        extra
            .iter()
            .take(10)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  "),
    );
}
