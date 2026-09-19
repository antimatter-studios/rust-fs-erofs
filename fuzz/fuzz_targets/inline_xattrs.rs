#![no_main]
//! The inline xattr area behind an inode: a shared-entry index array
//! followed by entries whose name and value lengths are both declared
//! in the entry header.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((_, entries)) = fs_erofs::parse_inline_xattrs(data) {
        for entry in &entries {
            let _ = fs_erofs::resolve_full_name(entry.name_index, &entry.name);
        }
    }
});
