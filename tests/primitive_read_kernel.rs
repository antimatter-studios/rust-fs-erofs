//! `primitives::read_extent` against the Linux kernel (#311).
//!
//! On images `mkfs.erofs` built, reading every extent of every file's
//! walk and joining them must give the bytes the kernel reads, through
//! every layout and codec the guest's kernel mounts: plain, chunked,
//! LZ4 with ztailpacking, LZ4HC big pclusters, fragments and dedupe.
//! With one pcluster destroyed, each extent of each file is read by the
//! kernel on its own (`dd`): an extent the kernel reads must read to the
//! same bytes here, and one it cannot must be an error here or read to
//! the bytes the file was built with. The kernel reads whole pages, so a
//! page shared with the destroyed pcluster fails there as a whole; this
//! reads by extent, so it may keep that page's other bytes, and those
//! must be the file's own.

mod common;

use common::{build_with_mkfs_erofs, dir, file, mixed_run_files, stage_image, MemDev};
use fs_erofs::primitives::{
    inspect_inode, inspect_superblock, read_extent, walk_data, DataWalk, ExtentSource,
};
use fs_erofs::superblock::Superblock;
use fs_erofs_test_support::{guest_kernel_read, sha256_hex};

fn files() -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = mixed_run_files().into_iter().take(16).collect();
    files.push(("small.txt".into(), b"hello small\n".to_vec()));
    files.push((
        "plain.bin".into(),
        (0..3 * 4096 + 100).map(|i| (i % 253) as u8).collect(),
    ));
    files
}

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let files = files();
    dir(files.iter().map(|(n, d)| (n.as_str(), file(d))).collect())
}

/// The kernel's inode number and SHA-256 of every file.
fn kernel_files(image: &std::path::Path) -> Vec<(String, u64, String)> {
    let mut script = String::from("cd \"$MNT\"\nfor f in");
    for (name, _) in files() {
        script.push(' ');
        script.push_str(&name);
    }
    script.push_str("; do\n    h=\"$(sha256sum < \"$f\")\"\n");
    script
        .push_str("    printf '%s %s %s\\n' \"$f\" \"$(stat -c %i \"$f\")\" \"${h%% *}\"\ndone\n");
    let out = guest_kernel_read(image.to_str().expect("utf-8 image path"), &script);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            (f[0].to_string(), f[1].parse().unwrap(), f[2].to_string())
        })
        .collect()
}

struct Image {
    source: MemDev,
    geometry: fs_erofs::primitives::Geometry,
    superblock: Superblock,
}

fn image(bytes: &[u8]) -> Image {
    let source = MemDev::new(bytes.to_vec());
    let evidence = inspect_superblock(&source);
    Image {
        geometry: evidence.geometry().expect("geometry"),
        superblock: evidence.fields.expect("fields"),
        source,
    }
}

fn walk(image: &Image, nid: u64) -> DataWalk {
    let inode = inspect_inode(&image.source, &image.geometry, nid)
        .inode
        .expect("inode");
    walk_data(
        &image.source,
        &image.geometry,
        Some(&image.superblock),
        &inode,
    )
}

#[test]
fn every_file_read_extent_by_extent_is_the_file_the_kernel_reads() {
    let mut problems = Vec::new();
    let mut kinds = std::collections::BTreeSet::new();
    for args in [
        &["-b4096"][..],
        &["-b4096", "--chunksize=8192"][..],
        &["-b4096", "-zlz4", "-Eztailpacking"][..],
        &["-b4096", "-zlz4hc", "-C65536"][..],
        &["-b4096", "-zlz4hc", "-Efragments,dedupe"][..],
    ] {
        let img = build_with_mkfs_erofs(args, &tree());
        let image = image(&img.bytes);
        for (name, nid, kernel) in kernel_files(&img.path) {
            let walk = walk(&image, nid);
            let mut bytes = Vec::new();
            for extent in &walk.extents {
                kinds.insert(match &extent.source {
                    ExtentSource::Plain { .. } => "plain",
                    ExtentSource::Inline { .. } => "inline",
                    ExtentSource::Hole => "hole",
                    ExtentSource::Pcluster {
                        algorithm: None, ..
                    } => "stored pcluster",
                    ExtentSource::Pcluster { .. } => "compressed pcluster",
                    ExtentSource::Fragment { .. } => "fragment",
                });
                match read_extent(
                    &image.source,
                    &image.geometry,
                    Some(&image.superblock),
                    nid,
                    extent,
                ) {
                    Ok(part) => bytes.extend(part),
                    Err(finding) => {
                        problems.push(format!("{args:?} {name}: {extent:?}: {}", finding.detail))
                    }
                }
            }
            if sha256_hex(&bytes) != kernel {
                problems.push(format!(
                    "{args:?} {name}: the extents read differently from the kernel"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    for kind in ["plain", "inline", "compressed pcluster", "fragment"] {
        assert!(kinds.contains(kind), "no {kind} extent was read: {kinds:?}");
    }
}

#[test]
fn a_destroyed_pcluster_is_missing_and_every_other_extent_reads_as_the_kernel_reads_it() {
    let img = build_with_mkfs_erofs(&["-b4096", "-zlz4"], &tree());
    let healthy = image(&img.bytes);
    let nids = kernel_files(&img.path);
    let nid = |name: &str| nids.iter().find(|(n, _, _)| n == name).unwrap().1;
    // The first compressed pcluster of f04.bin.
    let (offset, stored_len) = walk(&healthy, nid("f04.bin"))
        .extents
        .iter()
        .find_map(|e| match e.source {
            ExtentSource::Pcluster {
                offset,
                stored_len,
                algorithm: Some(_),
                inline: false,
                ..
            } => Some((offset, stored_len)),
            _ => None,
        })
        .expect("fixture: f04.bin has a compressed pcluster");
    let mut bytes = img.bytes.clone();
    bytes[offset as usize..(offset + stored_len) as usize].fill(0xFF);
    let damaged = image(&bytes);
    let (path, _guard) = stage_image("pcluster-damage", &bytes);

    // Every extent of every file, read by the kernel on its own.
    let mut script = String::from("cd \"$MNT\"\nwhile read -r f start len; do\n");
    script.push_str(
        "    if h=\"$(dd if=\"$f\" iflag=skip_bytes,count_bytes skip=\"$start\" \
         count=\"$len\" bs=65536 status=none | sha256sum)\"; \
         then echo \"${h%% *}\"; else echo ERR; fi\ndone <<'EXTENTS'\n",
    );
    let mut ours = Vec::new();
    for (name, nid, _) in &nids {
        for extent in walk(&damaged, *nid).extents {
            script.push_str(&format!("{name} {} {}\n", extent.logical, extent.len));
            let read = read_extent(
                &damaged.source,
                &damaged.geometry,
                Some(&damaged.superblock),
                *nid,
                &extent,
            );
            let built = &files().into_iter().find(|(n, _)| n == name).unwrap().1;
            let range = extent.logical as usize..(extent.logical + extent.len) as usize;
            ours.push((
                format!("{name}@{}+{}", extent.logical, extent.len),
                read.ok().map(|b| sha256_hex(&b)),
                sha256_hex(&built[range]),
            ));
        }
    }
    script.push_str("EXTENTS\n");
    let out = guest_kernel_read(path.to_str().unwrap(), &script);
    let kernel: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        kernel.len(),
        ours.len(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut missing = 0;
    let mut problems = Vec::new();
    for ((what, ours, built), kernel) in ours.iter().zip(&kernel) {
        let kernel = (kernel != "ERR").then_some(kernel);
        if ours.is_none() {
            missing += 1;
        }
        let sound = match (ours, kernel) {
            (Some(ours), Some(kernel)) => ours == kernel,
            (None, Some(_)) => false,
            (Some(ours), None) => ours == built,
            (None, None) => true,
        };
        if !sound {
            problems.push(format!("{what}: kernel {kernel:?}, read_extent {ours:?}"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    assert!(
        missing > 0 && missing < ours.len(),
        "fixture: {missing} of {} extents missing",
        ours.len()
    );
}
