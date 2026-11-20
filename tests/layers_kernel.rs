//! The layers against the Linux kernel, on images `mkfs.erofs` made (#311).
//!
//! The conformance helpers prove the shape of the contract; this proves
//! the layers read EROFS as Linux does. For plain, chunked, LZ4 with
//! ztailpacking, and LZ4HC with fragments and dedupe:
//!
//! - `check_healthy` passes with every file the kernel lists expected at
//!   the bytes the kernel reads;
//! - `FileAPI` lists every directory as the kernel does, with its sizes;
//! - the salvage tree gives every path the kernel walks a parent claim on
//!   the candidate of the directory the kernel finds it in, and the root
//!   the root claim;
//! - with a directory block destroyed, the volume opened for salvage
//!   passes `check_damaged`, and the damage report names that block.

mod common;

use common::{build_with_mkfs_erofs, dir, file, mixed_run_files, stage_image, MemDev};
use fs_core::layers::conformance::{check_damaged, check_healthy, Expected, Limits};
use fs_core::layers::{CandidateKind, FileAPI, OpenVolume, Parent, PrimitiveAPI};
use fs_core::BlockRead;
use fs_erofs::layers::ErofsVolume;
use fs_erofs_test_support::{guest_kernel_read, sha256_hex};
use std::collections::BTreeMap;
use std::sync::Arc;

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let files = mixed_run_files();
    let names: Vec<String> = (0..300).map(|i| format!("entry-{i:03}")).collect();
    dir(vec![
        ("hello.txt", file(b"hello\n")),
        (
            "data",
            dir(files.iter().map(|(n, d)| (n.as_str(), file(d))).collect()),
        ),
        (
            "wide",
            dir(names
                .iter()
                .map(|n| (n.as_str(), file(n.as_bytes())))
                .collect()),
        ),
        (
            "nested",
            dir(vec![("deeper", dir(vec![("leaf.txt", file(b"leaf\n"))]))]),
        ),
        ("empty", dir(vec![])),
    ])
}

/// Every path the kernel walks: path -> (inode, type, size, SHA-256 for
/// a file).
type Walk = BTreeMap<String, (u64, char, u64, Option<String>)>;

fn kernel_walk(image: &std::path::Path) -> Walk {
    let out = guest_kernel_read(
        image.to_str().expect("utf-8 image path"),
        r#"cd "$MNT"
find . -printf '%P|%i|%y|%s\n' | while IFS='|' read -r p i y s; do
    h=-
    if [ "$y" = f ]; then h="$(sha256sum < "$p")"; h="${h%% *}"; fi
    printf '/%s|%s|%s|%s|%s\n' "$p" "$i" "$y" "$s" "$h"
done"#,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split('|').collect();
            let path = if f[0] == "/" {
                "/".to_string()
            } else {
                f[0].to_string()
            };
            (
                path,
                (
                    f[1].parse().unwrap(),
                    f[2].chars().next().unwrap(),
                    f[3].parse().unwrap(),
                    (f[4] != "-").then(|| f[4].to_string()),
                ),
            )
        })
        .collect()
}

fn open(bytes: &[u8]) -> ErofsVolume {
    let source: Arc<dyn BlockRead> = MemDev::arc(bytes.to_vec());
    ErofsVolume::open(source).expect("open")
}

/// Whether an image shape lets one stored unit serve several files:
/// fragments put many files' tails in one packed pcluster, and dedupe
/// shares a pcluster between files.
fn shares_stored_units(args: &[&str]) -> bool {
    args.iter()
        .any(|a| a.contains("fragments") || a.contains("dedupe"))
}

/// The one way `primitive.walks` may fail on such an image, filed on
/// rust-fs-core#453: a walk maps more stored bytes than the file claims,
/// because the encoded unit holding its bytes also holds other files'.
/// Every listed walk must still be complete, inside the source, in file
/// order, within the claim, and rebuild exactly the claimed bytes.
fn only_shared_units_overmap(check: &fs_core::layers::conformance::Check) -> bool {
    if check.name != "primitive.walks" {
        return false;
    }
    let walks: Vec<&str> = check.detail.split("\"Extent").skip(1).collect();
    !walks.is_empty()
        && walks.iter().all(|walk| {
            let number = |after: &str, before: &str| -> Option<u64> {
                let rest = walk.split_once(after)?.1;
                rest.split_once(before)?.0.trim().parse().ok()
            };
            let claimed = number(" of ", " claimed");
            let mapped = number("mapped ", " of ");
            let plan = number("plan Some(", ")");
            walk.contains("end Complete, inside true, in file order true, within the claim true")
                && claimed.is_some()
                && plan == claimed
                && mapped > claimed
        })
}

const SHAPES: [&[&str]; 4] = [
    &["-b4096"],
    &["-b4096", "--chunksize=8192"],
    &["-b4096", "-zlz4", "-Eztailpacking"],
    &["-b4096", "-zlz4hc", "-Efragments,dedupe"],
];

#[test]
fn the_layers_read_mkfs_erofs_images_as_the_kernel_reads_them() {
    let built = tree();
    let built_files: BTreeMap<String, Vec<u8>> = {
        let mut out = BTreeMap::new();
        out.insert("/hello.txt".to_string(), b"hello\n".to_vec());
        out.insert("/nested/deeper/leaf.txt".to_string(), b"leaf\n".to_vec());
        for (n, d) in mixed_run_files() {
            out.insert(format!("/data/{n}"), d);
        }
        for i in 0..300 {
            let n = format!("entry-{i:03}");
            out.insert(format!("/wide/{n}"), n.into_bytes());
        }
        out
    };
    let limits = Limits {
        max_nodes: 8192,
        ..Limits::default()
    };
    for args in SHAPES {
        let img = build_with_mkfs_erofs(args, &built);
        let kernel = kernel_walk(&img.path);
        let volume = open(&img.bytes);

        // check_healthy, expecting what the kernel reads.
        let mut expected = Vec::new();
        for (path, (_, kind, _, hash)) in &kernel {
            match kind {
                'd' if path != "/" => expected.push(Expected::directory(path)),
                'f' => {
                    let data = &built_files[path];
                    assert_eq!(
                        Some(sha256_hex(data)),
                        *hash,
                        "{args:?} {path}: the kernel reads other bytes than were built"
                    );
                    expected.push(Expected::file(path, data));
                }
                _ => {}
            }
        }
        expected.push(Expected::absent("/not-there"));
        let report = check_healthy(&volume, &expected, &limits);
        let failed: Vec<_> = report
            .failures()
            .filter(|check| !(shares_stored_units(args) && only_shared_units_overmap(check)))
            .collect();
        assert!(failed.is_empty(), "{args:?}: {report}");

        // FileAPI lists each path at the kernel's inode and size.
        for (path, (ino, _, size, _)) in &kernel {
            let node = volume
                .lookup(path)
                .unwrap_or_else(|e| panic!("{args:?} {path}: {e}"));
            assert_eq!(node, *ino, "{args:?} {path}: inode");
            if !path.ends_with('/') && kernel[path].1 == 'f' {
                assert_eq!(
                    volume.metadata(&node).unwrap().size,
                    *size,
                    "{args:?} {path}"
                );
            }
        }

        // The salvage tree: each path's candidate claims the directory the
        // kernel finds it in.
        let salvage = volume.salvage().expect("salvage");
        let candidates = salvage.all_candidates().expect("candidates");
        let at: BTreeMap<u64, u64> = candidates
            .iter()
            .map(|c| (c.detail, c.location.offset))
            .collect();
        let tree = salvage.rebuild_tree(&candidates).expect("tree");
        let claims: BTreeMap<u64, Vec<Parent>> = tree
            .nodes
            .iter()
            .map(|n| (n.candidate, n.parents.iter().map(|c| c.parent).collect()))
            .collect();
        for (path, (ino, kind, _, _)) in &kernel {
            let Some(offset) = at.get(ino) else {
                panic!("{args:?} {path}: inode {ino} is not a candidate");
            };
            let want = if path == "/" {
                Parent::Root
            } else {
                let parent = path.rsplit_once('/').unwrap().0;
                let parent = if parent.is_empty() { "/" } else { parent };
                Parent::Candidate(at[&kernel[parent].0])
            };
            assert!(
                claims[offset].contains(&want),
                "{args:?} {path}: claims {:?}, not {want:?}",
                claims[offset]
            );
            let candidate = candidates.iter().find(|c| c.detail == *ino).unwrap();
            let kind_ok = match kind {
                'd' => candidate.kind == CandidateKind::Directory,
                'f' => candidate.kind == CandidateKind::File,
                _ => true,
            };
            assert!(kind_ok, "{args:?} {path}: {:?}", candidate.kind);
        }
    }
}

#[test]
fn a_damaged_mkfs_erofs_image_passes_the_damaged_checks_and_names_its_damage() {
    let img = build_with_mkfs_erofs(&["-b4096"], &tree());
    let kernel = kernel_walk(&img.path);
    let volume = open(&img.bytes);
    let wide = fs_erofs::Filesystem::open(MemDev::arc(img.bytes.clone()))
        .unwrap()
        .read_inode(kernel["/wide"].0)
        .unwrap();
    let block = u64::from(wide.raw_u) * 4096;
    drop(volume);
    let mut bytes = img.bytes.clone();
    bytes[block as usize..block as usize + 4096].fill(0xFF);
    let (image, _guard) = stage_image("layers-damage", &bytes);
    // The kernel still mounts it and reads the files outside the block.
    let damaged_walk = guest_kernel_read(
        image.to_str().unwrap(),
        "cat \"$MNT/hello.txt\" && ! ls \"$MNT/wide\" > /dev/null 2>&1 && echo OK",
    );
    assert_eq!(
        String::from_utf8_lossy(&damaged_walk.stdout)
            .trim_end()
            .lines()
            .last(),
        Some("OK")
    );

    let source: Arc<dyn BlockRead> = MemDev::arc(bytes);
    let volume = ErofsVolume::open_for_salvage(source).expect("salvage open");
    let report = check_damaged(&volume, &Limits::default());
    assert!(report.passed(), "{report}");
    let mut damage = Vec::new();
    volume
        .salvage()
        .unwrap()
        .report_damage(&mut damage)
        .expect("report");
    assert!(
        damage
            .iter()
            .any(|d| d.location.offset >= block && d.location.offset < block + 4096),
        "no damage reported in the destroyed block at {block}: {damage:#?}"
    );
}
