//! `primitives::walk_data` against the Linux kernel's FIEMAP (#311).
//!
//! `filefrag -v -b1` in the guest asks the kernel where each file's bytes
//! are. For every extent the kernel reports, the walk must map the
//! extent's first byte to the same place: the same source byte for data
//! stored as is, the same pcluster for compressed data, and an inline run
//! where the kernel flags the extent inline. Every walk must cover its
//! whole file. With one file's block address moved past the source, the
//! kernel cannot read the file and its walk must be broken.

mod common;

use common::{build_with_mkfs_erofs, dir, file, mixed_run_files, stage_image, MemDev};
use fs_erofs::primitives::{
    inspect_inode, inspect_superblock, walk_data, DataWalk, ExtentSource, WalkEnd,
};
use fs_erofs_test_support::guest_kernel_read;

fn files() -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = mixed_run_files().into_iter().take(12).collect();
    files.push(("small.txt".into(), b"hello small\n".to_vec()));
    files.push((
        "plain.bin".into(),
        (0..3 * 4096 + 100).map(|i| (i % 253) as u8).collect(),
    ));
    files.push(("empty".into(), Vec::new()));
    files
}

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let files = files();
    dir(files.iter().map(|(n, d)| (n.as_str(), file(d))).collect())
}

/// One extent as FIEMAP reports it.
#[derive(Debug)]
struct KernelExtent {
    logical: u64,
    physical: u64,
    inline: bool,
}

/// Each file's inode number and the kernel's extents for it.
fn kernel_extents(image: &std::path::Path) -> Vec<(String, u64, Vec<KernelExtent>)> {
    let mut script = String::from("cd \"$MNT\"\nfor f in");
    for (name, _) in files() {
        script.push(' ');
        script.push_str(&name);
    }
    script.push_str("; do\n    printf '== %s %s\\n' \"$f\" \"$(stat -c %i \"$f\")\"\n");
    script.push_str("    filefrag -v -b1 \"$f\"\ndone\n");
    let out = guest_kernel_read(image.to_str().expect("utf-8 image path"), &script);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "filefrag:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut result: Vec<(String, u64, Vec<KernelExtent>)> = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("== ") {
            let (name, ino) = rest.split_once(' ').unwrap();
            result.push((name.to_string(), ino.parse().unwrap(), Vec::new()));
            continue;
        }
        // "   0:        0..     999:       1600..      2599:   1000:   flags"
        let fields: Vec<&str> = line.split(':').map(str::trim).collect();
        if fields.len() < 4 || fields[0].parse::<u64>().is_err() {
            continue;
        }
        let start = |range: &str| {
            range
                .split("..")
                .next()
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
        };
        // The `expected` column is empty unless the extent is not where
        // the one before it ended, so the flags are the fifth or sixth
        // field: whatever follows the length.
        let flags = fields.get(4..).map(|f| f.join(",")).unwrap_or_default();
        result.last_mut().unwrap().2.push(KernelExtent {
            logical: start(fields[1]),
            physical: start(fields[2]),
            inline: flags.split(',').any(|f| f == "inline"),
        });
    }
    result
}

fn walk(bytes: &[u8], nid: u64) -> DataWalk {
    let source = MemDev::new(bytes.to_vec());
    let evidence = inspect_superblock(&source);
    let geometry = evidence.geometry().expect("geometry");
    let inode = inspect_inode(&source, &geometry, nid).inode.expect("inode");
    walk_data(&source, &geometry, evidence.fields.as_ref(), &inode)
}

/// Where the walk puts file byte `logical`: a source byte, a pcluster's
/// first byte, or "inline".
fn place(walk: &DataWalk, logical: u64) -> Option<(Option<u64>, bool)> {
    let extent = walk
        .extents
        .iter()
        .find(|e| logical >= e.logical && logical < e.logical + e.len)?;
    Some(match &extent.source {
        ExtentSource::Plain { offset, .. } => (Some(offset + (logical - extent.logical)), false),
        ExtentSource::Inline { .. } => (None, true),
        ExtentSource::Pcluster { offset, .. } => (Some(*offset), false),
        ExtentSource::Hole | ExtentSource::Fragment { .. } => (None, false),
    })
}

#[test]
fn every_extent_the_kernel_reports_is_where_the_walk_puts_it() {
    let mut problems = Vec::new();
    let mut compared = std::collections::BTreeSet::new();
    for args in [
        &["-b4096"][..],
        &["-b4096", "--chunksize=8192"][..],
        &["-b4096", "-zlz4", "-Eztailpacking"][..],
        &["-b4096", "-zlz4hc", "-C65536"][..],
    ] {
        let img = build_with_mkfs_erofs(args, &tree());
        let sizes: std::collections::BTreeMap<String, u64> = files()
            .into_iter()
            .map(|(n, d)| (n, d.len() as u64))
            .collect();
        for (name, nid, extents) in kernel_extents(&img.path) {
            let walk = walk(&img.bytes, nid);
            let covered: u64 = walk.extents.iter().map(|e| e.len).sum();
            if walk.end != WalkEnd::Complete || covered != sizes[&name] {
                problems.push(format!(
                    "{args:?} {name}: walk ends {:?} covering {covered} of {}",
                    walk.end, sizes[&name]
                ));
                continue;
            }
            for kernel in &extents {
                let ours = place(&walk, kernel.logical);
                let extent = walk
                    .extents
                    .iter()
                    .find(|e| kernel.logical >= e.logical && kernel.logical < e.logical + e.len);
                if let Some(extent) = extent {
                    compared.insert(match extent.source {
                        ExtentSource::Plain { .. } => "plain",
                        ExtentSource::Inline { .. } => "inline",
                        ExtentSource::Pcluster { inline: true, .. } => "inline pcluster",
                        ExtentSource::Pcluster { .. } => "pcluster",
                        _ => "other",
                    });
                }
                let agrees = match ours {
                    Some((_, true)) => kernel.inline,
                    Some((Some(physical), false)) => !kernel.inline && physical == kernel.physical,
                    _ => false,
                };
                if !agrees {
                    problems.push(format!(
                        "{args:?} {name}: the kernel puts byte {} at {} (inline {}), the walk at {ours:?}",
                        kernel.logical, kernel.physical, kernel.inline
                    ));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    for kind in ["plain", "inline", "pcluster", "inline pcluster"] {
        assert!(
            compared.contains(kind),
            "no {kind} extent was compared: {compared:?}"
        );
    }
}

#[test]
fn a_block_address_past_the_source_breaks_the_walk_where_the_kernel_cannot_read() {
    let img = build_with_mkfs_erofs(&["-b4096"], &tree());
    let nid = kernel_extents(&img.path)
        .into_iter()
        .find(|(name, _, _)| name == "plain.bin")
        .expect("plain.bin")
        .1;
    let source = MemDev::new(img.bytes.clone());
    let geometry = inspect_superblock(&source).geometry().unwrap();
    let at = geometry.inode_offset(nid) as usize;
    let mut bytes = img.bytes.clone();
    bytes[at + 0x10..at + 0x14].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    let (image, _guard) = stage_image("data-damage", &bytes);
    let out = guest_kernel_read(
        image.to_str().unwrap(),
        "if cat \"$MNT/plain.bin\" > /dev/null 2>&1; then echo READ; else echo ERR; fi",
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "ERR",
        "the kernel reads it"
    );
    let walk = walk(&bytes, nid);
    assert!(
        matches!(walk.end, WalkEnd::Broken(_)) && walk.extents.is_empty(),
        "{walk:?}"
    );
}
