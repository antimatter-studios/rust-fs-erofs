//! EROFS directory iteration.
//!
//! A directory's data is one or more dir-blocks (size = `1 <<
//! sb.dirblkbits`, defaulting to the FS block size when `dirblkbits == 0`).
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
/// than panicking.
pub fn iter_block(block: &[u8]) -> Result<Vec<DirEntry>> {
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
    let n_dirents = first_nameoff / EROFS_DIRENT_SIZE;

    let mut out = Vec::with_capacity(n_dirents);
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

        out.push(DirEntry {
            nid,
            file_type,
            name: block[nameoff..name_end].to_vec(),
        });
    }
    Ok(out)
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

    #[test]
    fn rejects_corrupt_nameoff() {
        let mut buf = synth_dir_block(&[(36, ftype::DIR, b".")], 4096);
        // Stomp the first nameoff to a value that isn't a multiple of 12.
        buf[8..10].copy_from_slice(&5u16.to_le_bytes());
        assert!(matches!(iter_block(&buf), Err(Error::BadDirent(_))));
    }
}
