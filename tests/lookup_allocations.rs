//! Resolving one name does not decode and allocate the whole directory
//! (#61).
//!
//! `Filesystem::lookup` called `read_dir`, which returns a `Vec<DirEntry>`
//! whose every entry owns its name as a fresh `Vec<u8>`, and then compared
//! one of them. So resolving a single path component allocated once per
//! entry in the directory, and `lookup_path` paid that per component. The
//! device-read harness in `tests/read_path_cost.rs` cannot see it: with the
//! metadata cache warm those lookups reach the device zero times.
//!
//! So this counts heap allocations on the calling thread instead. The
//! count is thread-local so the test harness's own threads do not leak into
//! it, and a control shows the counter does see `read_dir`'s allocations.

mod common;
use common::{dir, file, open_image};

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with`: the thread-local may already be torn down while a
        // thread exits, and an allocator must not panic.
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|on| on.set(true));
    let out = f();
    COUNTING.with(|on| on.set(false));
    (out, ALLOCS.with(|n| n.get()))
}

const ENTRIES: usize = 2000;

#[test]
fn looking_up_one_name_does_not_allocate_per_directory_entry() {
    let names: Vec<String> = (0..ENTRIES).map(|i| format!("entry-{i:05}.txt")).collect();
    let tree = dir(names.iter().map(|n| (n.as_str(), file(b"x"))).collect());
    let fs = open_image(fs_erofs::mkfs::build_image(tree, 12).expect("build"));
    let root = fs.root_inode().expect("root");

    // Control: the counter sees per-entry allocation when it happens.
    let (listing, listing_allocs) = allocations_during(|| fs.read_dir(&root).expect("read_dir"));
    assert_eq!(listing.len(), ENTRIES + 2, "the directory has . and .. too");
    assert!(
        listing_allocs >= ENTRIES,
        "control: read_dir owns one name per entry, but only {listing_allocs} allocations \
         were counted -- the counter is not seeing this thread"
    );

    // Every name must still resolve, first to last.
    for name in [&names[0], &names[ENTRIES / 2], &names[ENTRIES - 1]] {
        let (found, allocs) = allocations_during(|| fs.lookup(&root, name.as_bytes()));
        let inode = found.unwrap_or_else(|e| panic!("lookup {name}: {e}"));
        assert!(inode.is_regular_file());
        assert!(
            allocs < 64,
            "looking up {name} in a {ENTRIES}-entry directory made {allocs} allocations; \
             resolving one name should not cost one per entry"
        );
    }
    assert!(matches!(
        fs.lookup(&root, b"not-there"),
        Err(fs_erofs::Error::NotFound)
    ));
}
