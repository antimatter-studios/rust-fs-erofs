//! The stable-toolchain half of the fuzzing setup: replay the corpus,
//! then mutate it, and refuse if a decoder panics, hangs, or if the
//! suite quietly stopped doing any work.
//!
//! # Why there are two halves
//!
//! `fuzz/` holds `cargo-fuzz` targets. Those are the explorer: they run
//! for as long as you give them and find inputs nobody thought of. They
//! cannot be a required check, because how long they ran decides what
//! they found, and a fresh discovery would fail whichever unrelated
//! pull request happened to be open.
//!
//! This suite is the gate. Deterministic, on the stable toolchain, in
//! every pull request, reading the same `fuzz/corpus/` the explorer
//! does. Anything the explorer finds is committed there and replayed
//! here from then on.
//!
//! # Why the corpus is whole images
//!
//! EROFS is read-only and mounted from sources the reader did not
//! produce -- a container image, an appliance, an OTA payload. The
//! interesting decoders are not the ones taking a byte slice: `zmap`,
//! `chunked` and the xattr readers all take a device, and are reached
//! only by opening an image and walking it. So the corpus holds five
//! images `mkfs.erofs` wrote -- one per compression algorithm, plus a
//! chunk-based one, since the algorithm id and the datalayout each
//! select a different decode path -- and `image` mutates and walks
//! them.
//!
//! The structures cut out of those same images seed the targets that do
//! take a byte slice, so a mutation of a superblock is a mutation of a
//! real superblock rather than of noise.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// The in-memory device and the bounded walk, shared verbatim with the
// explorer. See fuzz/shared/walk.rs for why it is included rather than
// depended on.
include!("../fuzz/shared/walk.rs");

/// Distinct starting points for the mutation stream. Fixed, so a
/// failure reproduces from the message alone.
const SEEDS: u64 = 6;

/// Mutated cases per (corpus file, seed) pair. Lower than the sibling
/// crates' because a case here opens and walks a whole filesystem
/// rather than parsing one structure.
const CASES_PER_SEED: usize = 96;

/// Below this, the suite is not doing its job.
const CASE_FLOOR: usize = 8_000;

/// Long enough that a loaded machine is never the reason, short enough
/// that a genuine hang is reported rather than left to the job timeout.
const DEADLINE: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------- targets

struct Target {
    corpus: &'static str,
    name: &'static str,
    run: fn(&[u8]),
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            corpus: "image",
            name: "image",
            run: walk,
        },
        Target {
            corpus: "superblock",
            name: "superblock",
            run: |b| {
                let _ = fs_erofs::Superblock::parse(b);
            },
        },
        Target {
            corpus: "inode",
            name: "inode",
            run: |b| {
                let _ = fs_erofs::Inode::parse(1, b);
            },
        },
        Target {
            corpus: "dir_block",
            name: "dir_block",
            run: |b| {
                let _ = fs_erofs::dir::iter_block(b);
                let _ = fs_erofs::dir::visit_block(b, |_, _, _| {});
            },
        },
        Target {
            corpus: "inline_xattrs",
            name: "inline_xattrs",
            run: |b| {
                if let Ok((_, entries)) = fs_erofs::parse_inline_xattrs(b) {
                    for entry in &entries {
                        let _ = fs_erofs::resolve_full_name(entry.name_index, &entry.name);
                    }
                }
            },
        },
    ]
}

// ---------------------------------------------------------------- corpus

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus")
}

fn seeds(corpus: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_root().join(corpus);
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!(
                "reading the corpus directory {}: {e}\n\
                 If this says the directory is not there, check .gitignore: a corpus \
                 image can be swallowed by a pattern meant for build output, and the \
                 commit will look complete.",
                dir.display()
            )
        })
        .map(|entry| {
            let path = entry.expect("corpus directory entry").path();
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("reading the seed {}: {e}", path.display()));
            let name = path
                .file_name()
                .expect("seed file name")
                .to_string_lossy()
                .into_owned();
            (name, bytes)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------- mutation

/// xorshift64*. Small, deterministic, and not a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of a real structure or a real image, preserving length.
///
/// Length is preserved because a device answers a read past its end
/// with `ShortRead` before any of this code is reached -- a hostile
/// image controls what is in a block, not how many bytes the device
/// hands back.
///
/// The `header` bias exists because an image is mostly file data: a
/// uniformly random offset in a 48 KiB image lands in somebody's text
/// file nine times out of ten, where nothing parses it. Half the
/// mutations are aimed at the first two blocks, which is where the
/// superblock, the inode table and the directory blocks are.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    let metadata_end = out.len().min(8192);
    let region = if rng.next() & 1 == 0 {
        metadata_end
    } else {
        out.len()
    };

    match rng.below(5) {
        0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(region);
                out[at] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            let at = rng.below(region);
            let len = 1 + rng.below(16.min(out.len() - at));
            let fill = if rng.next() & 1 == 0 { 0x00 } else { 0xff };
            out[at..at + len].fill(fill);
        }
        2 => {
            let width = [2usize, 4, 8][rng.below(3)];
            if out.len() >= width {
                let at = rng.below(region.saturating_sub(width) + 1) & !(width - 1);
                if at + width <= out.len() {
                    let value: u64 = match rng.below(4) {
                        0 => 0,
                        1 => 1,
                        2 => u64::MAX,
                        _ => rng.next(),
                    };
                    // Little-endian: every multi-byte field in EROFS is.
                    out[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
                }
            }
        }
        3 => {
            if out.len() >= 8 {
                let a = rng.below(region / 4) * 4;
                let b = rng.below(region / 4) * 4;
                if a + 4 <= out.len() && b + 4 <= out.len() {
                    for i in 0..4 {
                        out.swap(a + i, b + i);
                    }
                }
            }
        }
        _ => {
            if out.len() >= 4 {
                let at = rng.below(region / 4) * 4;
                if at + 4 <= out.len() {
                    let word = u32::from_le_bytes(out[at..at + 4].try_into().expect("4 bytes"));
                    let delta = [1i64, -1, 2, -2, 255, -255][rng.below(6)];
                    let changed = (i64::from(word).wrapping_add(delta)) as u32;
                    out[at..at + 4].copy_from_slice(&changed.to_le_bytes());
                }
            }
        }
    }
    out
}

/// The case in flight, readable even if the lock was poisoned by the
/// panic we are trying to describe.
fn describe(current: &Arc<Mutex<String>>) -> String {
    match current.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn every_target_has_a_corpus() {
    for target in targets() {
        assert!(
            !seeds(target.corpus).is_empty(),
            "the target {} reads fuzz/corpus/{}, which holds no seeds -- a target with an \
             empty corpus runs no cases and would pass in silence. Rebuild it with \
             scripts/make-fuzz-corpus.sh",
            target.name,
            target.corpus,
        );
    }
}

/// The corpus is an oracle, not just fuel: every committed image is one
/// `mkfs.erofs` wrote and `fsck.erofs` accepted, so this crate must be
/// able to read all of it. A seed that stopped opening would otherwise
/// go on being mutated and go on not failing, because a mutation of an
/// unreadable image is also unreadable.
#[test]
fn every_committed_image_opens_and_lists_its_root() {
    let images = seeds("image");
    assert!(
        images.len() >= 5,
        "only {} images; the corpus has shrunk",
        images.len()
    );
    for (name, bytes) in images {
        let dev: Arc<dyn fs_core::BlockRead> = Arc::new(Bytes(bytes));
        let fs = fs_erofs::Filesystem::open(dev)
            .unwrap_or_else(|e| panic!("{name}: an image mkfs.erofs wrote would not open: {e}"));
        let root = fs
            .root_inode()
            .unwrap_or_else(|e| panic!("{name}: the root inode would not read: {e}"));
        let entries = fs
            .read_dir(&root)
            .unwrap_or_else(|e| panic!("{name}: the root directory would not list: {e}"));
        assert!(
            entries.len() > 240,
            "{name}: the root listed {} entries, fewer than the corpus tree has -- the \
             image is not the one the script builds",
            entries.len()
        );
    }
}

#[test]
fn deterministic_mutations_of_real_images_are_survived() {
    let cases = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(String::from("(not started)")));
    let (done_tx, done_rx) = mpsc::channel();

    let hook_current = Arc::clone(&current);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("\nfuzz gate: panicked at {}", describe(&hook_current));
        previous_hook(info);
    }));

    let worker_cases = Arc::clone(&cases);
    let worker_current = Arc::clone(&current);
    let worker = std::thread::spawn(move || {
        for target in targets() {
            for (seed_name, bytes) in seeds(target.corpus) {
                for start in 0..SEEDS {
                    let mut rng = Rng::new(start);
                    for case in 0..CASES_PER_SEED {
                        *worker_current.lock().expect("progress lock") =
                            format!("{} / {seed_name} / seed {start} / case {case}", target.name);
                        let mutated = mutate(&bytes, &mut rng);
                        (target.run)(&mutated);
                        worker_cases.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // A timeout means the worker is still running: a hang. A disconnect
    // means it panicked, and the panic is what is worth reporting.
    match done_rx.recv_timeout(DEADLINE) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Written to the process's stderr rather than through
            // `eprintln!`, which the harness captures into a buffer it
            // only prints when a test finishes -- and exiting here means
            // it never finishes.
            let _ = writeln!(
                std::io::stderr(),
                "\nhung: no progress for {:?} at {}\n\
                 A decoder did not return. A tree that points at itself, or a cluster map \
                 that never advances, looks exactly like this.",
                DEADLINE,
                describe(&current),
            );
            let _ = std::io::stderr().flush();
            std::process::exit(1);
        }
    }

    let outcome = worker.join();
    let _ = std::panic::take_hook();
    if outcome.is_err() {
        panic!("a decoder panicked at {}", describe(&current));
    }

    let total = cases.load(Ordering::Relaxed);
    assert!(
        total >= CASE_FLOOR,
        "only {total} mutated cases ran, below the floor of {CASE_FLOOR} -- the target \
         list or the corpus has collapsed, and a suite that runs nothing passes quickly",
    );
    eprintln!("{total} mutated cases");
}

#[test]
fn the_gate_covers_every_explorer_target() {
    // The two tiers drift apart the moment somebody adds a cargo-fuzz
    // target and forgets that nothing gates it on the stable toolchain.
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/Cargo.toml"))
            .expect("reading fuzz/Cargo.toml");

    let explorer: Vec<String> = manifest
        .lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .skip(1) // the package name is the first `name =` in the file
        .collect();

    assert!(
        !explorer.is_empty(),
        "fuzz/Cargo.toml declares no [[bin]] targets",
    );

    let gated: Vec<&str> = targets().iter().map(|t| t.name).collect();
    for name in &explorer {
        assert!(
            gated.contains(&name.as_str()),
            "fuzz/fuzz_targets/{name}.rs has no counterpart in this suite, so nothing \
             replays its corpus on the stable toolchain and anything it finds would only \
             stay fixed for as long as somebody keeps running the fuzzer by hand",
        );
    }
}
