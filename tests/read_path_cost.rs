//! What a read costs, in calls to the device.
//!
//! # Why this is a test and not a benchmark
//!
//! The number that matters is not wall time. Wall time on a laptop with
//! a warm page cache says more about the laptop than the driver: run it
//! twice and the second is faster for reasons this repository does not
//! control. **Calls to the device** are deterministic — the same image
//! walked the same way makes the same calls every time — so they can be
//! asserted on, and a change that makes the driver ask for more is a
//! regression a test can catch rather than a number somebody has to
//! remember.
//!
//! Wall time is printed beside them, because it is what a user feels,
//! and ignored by the assertions.
//!
//! # What is measured
//!
//! Three shapes, because they cost differently and a change can improve
//! one while ruining another:
//!
//! - **walk** — every directory in the tree, listed. Metadata only.
//! - **stat** — every file resolved by path from the root. Metadata,
//!   repeatedly, over the same blocks.
//! - **read** — every file's contents.
//!
//! The second is the one a cache should transform: resolving `/a/b/c`
//! re-reads the root directory and every directory above the target,
//! once per path.
//!
//! # The fixture is built here
//!
//! No downloaded image and no `mkfs.erofs` on `PATH`: the crate writes
//! its own images, so the tree below is built in memory, written once,
//! and measured. That makes the numbers reproducible on any machine and
//! keeps the test from skipping on a fresh clone.

mod common;
use common::{dir, file};

use fs_core::{CountingDevice, FileDevice};
use fs_erofs::{mkfs, Filesystem};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// What one shape of read cost.
struct Cost {
    reads: u64,
    bytes: u64,
    micros: u128,
    /// How much work was actually done, so a number that fell because
    /// the driver did less is not read as a number that fell because
    /// the driver got better.
    items: usize,
}

/// What one pass measured, so the two can be compared rather than each
/// asserting on itself.
struct Pass {
    walk: Cost,
    stat: Cost,
    read: Cost,
}

/// A tree wide and deep enough that resolving a path costs several
/// directory reads, which is the shape a cache is for. Contents are
/// derived from the name so nothing has to be remembered to check them.
fn fixture_bytes() -> Vec<u8> {
    // NAMES ARE HELD IN A VEC THAT OUTLIVES EACH `dir` CALL, because
    // `dir` takes `&str` and a `String` built in the loop would be gone
    // before the borrow was used.
    let names: Vec<String> = (0..6).map(|a| format!("t{a}")).collect();
    let mid_names: Vec<String> = (0..6).map(|b| format!("d{b}")).collect();

    let mut top = Vec::new();
    for (a, top_name) in names.iter().enumerate() {
        let leaf_names: Vec<Vec<String>> = (0..6)
            .map(|b| (0..6).map(|c| format!("f{a}{b}{c}.txt")).collect())
            .collect();
        let mut mid = Vec::new();
        for (b, mid_name) in mid_names.iter().enumerate() {
            let bodies: Vec<Vec<u8>> = leaf_names[b]
                .iter()
                .map(|n| n.repeat(64).into_bytes())
                .collect();
            let leaf: Vec<(&str, mkfs::Node)> = leaf_names[b]
                .iter()
                .zip(bodies.iter())
                .map(|(n, body)| (n.as_str(), file(body)))
                .collect();
            mid.push((mid_name.as_str(), dir(leaf)));
        }
        top.push((top_name.as_str(), dir(mid)));
    }
    mkfs::build_image(dir(top), 12).expect("build the fixture image")
}

/// The counter sits BELOW the cache, so what it reports is what
/// actually reached the device rather than what the driver asked for.
/// `blocks` of zero opens without a metadata cache, which is the
/// baseline.
fn open_counting(img: &Path, blocks: usize) -> (Filesystem, Arc<CountingDevice>) {
    let file = FileDevice::open(img).expect("open the fixture");
    let counting = Arc::new(CountingDevice::new(Arc::new(file)));
    let fs = Filesystem::open_with_cache(counting.clone(), blocks).expect("open");
    (fs, counting)
}

fn walk_paths(fs: &Filesystem, at: &str, depth: u32, out: &mut Vec<(String, bool)>) {
    if depth == 0 || out.len() > 4000 {
        return;
    }
    let Ok(inode) = fs.lookup_path(at) else {
        return;
    };
    let Ok(entries) = fs.read_dir(&inode) else {
        return;
    };
    for e in entries {
        if e.name == b"." || e.name == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(&e.name).to_string();
        let child = if at == "/" {
            format!("/{name}")
        } else {
            format!("{at}/{name}")
        };
        let Ok(child_inode) = fs.lookup_path(&child) else {
            continue;
        };
        let is_dir = child_inode.is_dir();
        out.push((child.clone(), is_dir));
        if is_dir {
            walk_paths(fs, &child, depth - 1, out);
        }
    }
}

fn measure<F>(counting: &CountingDevice, items: usize, body: F) -> Cost
where
    F: FnOnce(),
{
    counting.reset();
    let start = Instant::now();
    body();
    Cost {
        reads: counting.reads(),
        bytes: counting.bytes(),
        micros: start.elapsed().as_micros(),
        items,
    }
}

fn report(what: &str, c: &Cost) {
    let per = if c.items == 0 {
        0.0
    } else {
        c.reads as f64 / c.items as f64
    };
    eprintln!(
        "{what:<6} {:>6} reads  {:>9} bytes  {:>8} µs  over {:>4} items  ({per:.1} reads/item)",
        c.reads, c.bytes, c.micros, c.items
    );
}

/// The measurement itself. Prints the numbers and asserts only that the
/// driver did the work and that the cache did not make it ask for more
/// — the figures are recorded in `docs/read-path-cost.md` and compared
/// by hand when something changes, because a threshold baked in here
/// would either be so loose it catches nothing or so tight it fails on
/// a fixture rebuild.
#[test]
fn what_a_read_costs_in_calls_to_the_device() {
    let bytes = fixture_bytes();
    let tmp = std::env::temp_dir().join(format!(
        "erofs_read_path_cost_{}_{}.img",
        std::process::id(),
        bytes.len()
    ));
    std::fs::write(&tmp, &bytes).expect("write the fixture");
    eprintln!("measuring {} ({} bytes)", tmp.display(), bytes.len());

    eprintln!("--- uncached ---");
    let uncached = measure_one(&tmp, 0);
    eprintln!("--- cached ---");
    let cached = measure_one(&tmp, 512);
    let _ = std::fs::remove_file(&tmp);

    // THE ASSERTIONS ARE ON THE UNCACHED PASS, because it is the one
    // that must reach the device: if the counter reports nothing there,
    // it is not wired to the mount and every figure above is fiction.
    // The cached pass is allowed to reach zero, so asserting the same
    // of it would be asserting that the cache failed.
    assert!(
        uncached.walk.items > 0,
        "the fixture had nothing to walk — the measurement is of nothing"
    );
    assert!(
        uncached.walk.reads > 0 && uncached.stat.reads > 0,
        "no calls reached the device, so the counter is not wired to the mount"
    );
    for (what, un, ca) in [
        ("walk", &uncached.walk, &cached.walk),
        ("stat", &uncached.stat, &cached.stat),
        ("read", &uncached.read, &cached.read),
    ] {
        assert!(
            ca.reads <= un.reads,
            "{what}: the cache made it ask for more ({} vs {})",
            ca.reads,
            un.reads
        );
        assert_eq!(
            ca.items, un.items,
            "{what}: the two passes did different amounts of work, so the \
             figures are not comparable"
        );
    }
}

fn measure_one(img: &Path, blocks: usize) -> Pass {
    let (fs, counting) = open_counting(img, blocks);

    let mut paths = Vec::new();
    let walk = measure(&counting, 0, || walk_paths(&fs, "/", 8, &mut paths));
    let walk = Cost {
        items: paths.len(),
        ..walk
    };
    report("walk", &walk);

    let files: Vec<String> = paths
        .iter()
        .filter(|(_, is_dir)| !*is_dir)
        .map(|(p, _)| p.clone())
        .collect();

    // RESOLVING THE SAME PREFIXES AGAIN AND AGAIN is the shape a cache
    // is for: every path here walks the root and each directory above
    // its target.
    let stat = measure(&counting, files.len(), || {
        for p in &files {
            let _ = fs.lookup_path(p);
        }
    });
    report("stat", &stat);

    let read = measure(&counting, files.len(), || {
        for p in &files {
            if let Ok(inode) = fs.lookup_path(p) {
                let mut buf = vec![0u8; inode.size as usize];
                let _ = fs.read_file(&inode, 0, &mut buf);
            }
        }
    });
    report("read", &read);

    Pass { walk, stat, read }
}
