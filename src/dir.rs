//! EROFS directory iteration.
//!
//! A directory's data is one or more dir-blocks, each the size of a
//! filesystem block. `sb.dirblkbits` must be zero (what erofs-utils
//! writes); `Superblock::parse` refuses any other value, including one
//! equal to `blkszbits`, rather than walk directories at a granularity
//! the kernel does not use.
//! Each block packs:
//!
//! ```text
//! [dirent[0]][dirent[1]]...[dirent[N-1]][name0][name1]...[nameN-1]
//! ```
//!
//! `dirent[0].nameoff` is the byte offset of the first name from the
//! start of the block, which doubles as the end-of-array marker:
//! `N == nameoff[0] / sizeof(erofs_dirent)`.
//!
//! Each `erofs_dirent` is 12 bytes:
//!
//! - 0x00: `nid` (u64)        — inode NID
//! - 0x08: `nameoff` (u16)    — byte offset of name within this block
//! - 0x0A: `file_type` (u8)   — DT_REG/DT_DIR/...
//! - 0x0B: reserved (u8)
//!
//! Names are NOT null-terminated. Length is `nameoff[i+1] - nameoff[i]`,
//! or for the last entry, the block-local end of valid bytes (we stop at
//! the first NUL since EROFS pads tail bytes with zero).
//!
//! Source: `struct erofs_dirent` in `linux/fs/erofs/erofs_fs.h`.

use crate::error::{Error, Result};

pub const EROFS_DIRENT_SIZE: usize = 12;

/// File-type byte values. Match Linux `FT_*` constants -- carried
/// verbatim from the on-disk dirent.
#[allow(dead_code)]
pub mod ftype {
    pub const UNKNOWN: u8 = 0;
    pub const REG_FILE: u8 = 1;
    pub const DIR: u8 = 2;
    pub const CHRDEV: u8 = 3;
    pub const BLKDEV: u8 = 4;
    pub const FIFO: u8 = 5;
    pub const SOCK: u8 = 6;
    pub const SYMLINK: u8 = 7;
}

/// The dirent type byte for a POSIX mode.
///
/// The crate had three independent encodings of this taxonomy: the
/// `S_IF*` bits in `inode.rs`, the `ftype` values above, and — in
/// `capi.rs` — a third table that re-derived the mapping between them
/// in *octal*, referring to both sets by name in comments because it
/// referenced neither in code. This is that mapping, once, in the
/// module that owns the values it produces.
///
/// An unrecognised type is [`ftype::UNKNOWN`] rather than an error: a
/// dirent whose type byte disagrees with its inode's mode is something
/// a reader reports, not something it refuses to list.
pub fn dirent_type_for_mode(mode: u16) -> u8 {
    use crate::inode::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};
    match mode & S_IFMT {
        S_IFREG => ftype::REG_FILE,
        S_IFDIR => ftype::DIR,
        S_IFCHR => ftype::CHRDEV,
        S_IFBLK => ftype::BLKDEV,
        S_IFIFO => ftype::FIFO,
        S_IFSOCK => ftype::SOCK,
        S_IFLNK => ftype::SYMLINK,
        _ => ftype::UNKNOWN,
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub nid: u64,
    pub file_type: u8,
    pub name: Vec<u8>,
}

/// Walk every dirent in a single dir-block buffer.
///
/// Bounds-checked: a malformed `nameoff` (not a multiple of 12, beyond
/// block end, or before the previous nameoff) returns `BadDirent` rather
/// than panicking. So does a name that is empty, contains a NUL byte, or
/// contains `/` -- none of which a directory can legitimately list, and
/// each of which a consumer of the name would misread.
pub fn iter_block(block: &[u8]) -> Result<Vec<DirEntry>> {
    let mut out = Vec::with_capacity(dirent_count(block)?);
    visit_block(block, |nid, file_type, name| {
        out.push(DirEntry {
            nid,
            file_type,
            name: name.to_vec(),
        })
    })?;
    Ok(out)
}

/// How many dirents a directory block declares, from the first entry's
/// name offset, which the dirent array runs up to.
fn dirent_count(block: &[u8]) -> Result<usize> {
    if block.len() < EROFS_DIRENT_SIZE {
        return Err(Error::BadDirent("block shorter than one dirent"));
    }
    let first_nameoff = u16::from_le_bytes(block[8..10].try_into().unwrap()) as usize;
    if first_nameoff < EROFS_DIRENT_SIZE
        || first_nameoff > block.len()
        || !first_nameoff.is_multiple_of(EROFS_DIRENT_SIZE)
    {
        return Err(Error::BadDirent("first nameoff invalid"));
    }
    Ok(first_nameoff / EROFS_DIRENT_SIZE)
}

/// The same walk and the same validation as [`iter_block`], handing each
/// dirent's name to `f` as a borrowed slice instead of an owned copy.
///
/// Every entry in the block is validated before this returns `Ok`, so a
/// caller looking for one name sees exactly the errors a full listing
/// would, without allocating per entry (#61).
///
/// # Partial calls on error
///
/// Each entry is handed to `f` as soon as it has been validated, so when
/// a later entry is malformed `f` has ALREADY run for the valid ones
/// before it and this returns `Err`. A caller whose callback has effects
/// beyond its own local state must discard them on error -- which is what
/// [`iter_block`] and `Filesystem::lookup` do.
pub fn visit_block(block: &[u8], mut f: impl FnMut(u64, u8, &[u8])) -> Result<()> {
    let n_dirents = dirent_count(block)?;
    let first_nameoff = n_dirents * EROFS_DIRENT_SIZE;

    for i in 0..n_dirents {
        let off = i * EROFS_DIRENT_SIZE;
        let nid = u64::from_le_bytes(block[off..off + 8].try_into().unwrap());
        let nameoff = u16::from_le_bytes(block[off + 8..off + 10].try_into().unwrap()) as usize;
        let file_type = block[off + 10];

        if nameoff < first_nameoff || nameoff > block.len() {
            return Err(Error::BadDirent("dirent nameoff out of bounds"));
        }

        let name_end = if i + 1 < n_dirents {
            let next_off = (i + 1) * EROFS_DIRENT_SIZE;
            let next_nameoff =
                u16::from_le_bytes(block[next_off + 8..next_off + 10].try_into().unwrap()) as usize;
            if next_nameoff < nameoff || next_nameoff > block.len() {
                return Err(Error::BadDirent("dirent nameoff non-monotonic"));
            }
            next_nameoff
        } else {
            // Last entry: name runs to first NUL or block end.
            let mut e = nameoff;
            while e < block.len() && block[e] != 0 {
                e += 1;
            }
            e
        };

        let name = &block[nameoff..name_end];
        // The same three names `mkfs::build_image` refuses to write. `.`
        // and `..` are real on-disk dirents in EROFS and are NOT refused;
        // the C ABI filters them at its own boundary.
        if name.is_empty() {
            return Err(Error::BadDirent("dirent name is empty"));
        }
        if name.contains(&0) {
            // Only reachable for a non-last entry, whose name runs to the
            // next nameoff; it would reach the C ABI with `name_len`
            // disagreeing with `strlen(name)`.
            return Err(Error::BadDirent("dirent name contains a NUL byte"));
        }
        if name.contains(&b'/') {
            // A listed name that composes into a different path.
            return Err(Error::BadDirent("dirent name contains '/'"));
        }

        f(nid, file_type, name);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a directory block with the given (nid, file_type, name)
    /// entries. Pads to `block_size` bytes with zeros.
    pub(crate) fn synth_dir_block(entries: &[(u64, u8, &[u8])], block_size: usize) -> Vec<u8> {
        let n = entries.len();
        let header_len = n * EROFS_DIRENT_SIZE;
        let total_name_len: usize = entries.iter().map(|(_, _, n)| n.len()).sum();
        assert!(header_len + total_name_len <= block_size);

        let mut buf = vec![0u8; block_size];
        // Lay out names contiguously after the header.
        let mut name_cursor = header_len;
        for (i, (nid, ft, name)) in entries.iter().enumerate() {
            let off = i * EROFS_DIRENT_SIZE;
            buf[off..off + 8].copy_from_slice(&nid.to_le_bytes());
            buf[off + 8..off + 10].copy_from_slice(&(name_cursor as u16).to_le_bytes());
            buf[off + 10] = *ft;
            buf[name_cursor..name_cursor + name.len()].copy_from_slice(name);
            name_cursor += name.len();
        }
        buf
    }

    #[test]
    fn iter_two_entries() {
        let buf = synth_dir_block(&[(36, ftype::DIR, b"."), (36, ftype::DIR, b"..")], 4096);
        let entries = iter_block(&buf).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b".");
        assert_eq!(entries[1].name, b"..");
        assert_eq!(entries[0].file_type, ftype::DIR);
    }

    #[test]
    fn iter_three_entries_with_long_names() {
        let buf = synth_dir_block(
            &[
                (10, ftype::DIR, b"."),
                (11, ftype::DIR, b".."),
                (42, ftype::REG_FILE, b"hello.txt"),
            ],
            4096,
        );
        let entries = iter_block(&buf).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].name, b"hello.txt");
        assert_eq!(entries[2].nid, 42);
    }

    /// `iter_block` must refuse the name, with the reason naming the rule.
    fn refused_because(entries: &[(u64, u8, &[u8])], rule: &str) {
        let buf = synth_dir_block(entries, 4096);
        match iter_block(&buf) {
            Err(Error::BadDirent(why)) => assert!(
                why.contains(rule),
                "refused, but for {why:?} rather than the {rule:?} rule"
            ),
            other => panic!("a dirent name breaking the {rule:?} rule was accepted: {other:?}"),
        }
    }

    // #92: an embedded NUL reaches the C ABI with `name_len` disagreeing
    // with `strlen(name)`. Only a non-last entry can carry one, because
    // the last entry's name stops at its first NUL.
    #[test]
    fn refuses_a_name_containing_a_nul() {
        refused_because(
            &[(1, ftype::REG_FILE, b"a\0b"), (2, ftype::REG_FILE, b"z")],
            "NUL",
        );
    }

    // #92: a `/` makes a listed name compose into a different path.
    #[test]
    fn refuses_a_name_containing_a_slash() {
        refused_because(
            &[(1, ftype::REG_FILE, b"a/b"), (2, ftype::REG_FILE, b"z")],
            "'/'",
        );
        refused_because(
            &[(1, ftype::REG_FILE, b"z"), (2, ftype::REG_FILE, b"../x")],
            "'/'",
        );
    }

    // #92: `nameoff == next_nameoff` passes the offset checks and yields a
    // zero-byte name.
    #[test]
    fn refuses_an_empty_name() {
        refused_because(
            &[(1, ftype::REG_FILE, b""), (2, ftype::REG_FILE, b"z")],
            "empty",
        );
    }

    // The negative control: `.` and `..` are real on-disk dirents in EROFS
    // (unlike SquashFS), so the content checks must not refuse them.
    #[test]
    fn dot_and_dotdot_still_list() {
        let buf = synth_dir_block(
            &[
                (36, ftype::DIR, b"."),
                (36, ftype::DIR, b".."),
                (40, ftype::REG_FILE, b"name with spaces.txt"),
            ],
            4096,
        );
        let names: Vec<Vec<u8>> = iter_block(&buf)
            .expect("a well-formed directory must list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, [&b"."[..], b"..", b"name with spaces.txt"]);
    }

    #[test]
    fn rejects_corrupt_nameoff() {
        let mut buf = synth_dir_block(&[(36, ftype::DIR, b".")], 4096);
        // Stomp the first nameoff to a value that isn't a multiple of 12.
        buf[8..10].copy_from_slice(&5u16.to_le_bytes());
        assert!(matches!(iter_block(&buf), Err(Error::BadDirent(_))));
    }
}
