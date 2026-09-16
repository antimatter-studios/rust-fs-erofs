//! `dirblkbits` other than zero or the filesystem block size refuses the
//! image at open (#58).
//!
//! `read_dir` walks a directory in filesystem-block steps. `dirblkbits`
//! was parsed and read by nothing, so an image declaring a different
//! directory block size would have had its dirent arrays taken from the
//! wrong offsets -- a `BadDirent` or a listing assembled from unrelated
//! bytes, indistinguishable from corruption. erofs-utils writes zero on
//! every option combination measured on #58, which means "the filesystem
//! block size", which is what the reader does; the equal shift says the
//! same thing explicitly. Anything else is refused by name.
//!
//! The image is this crate's own, with the byte patched and the superblock
//! checksum recomputed so the case stays valid once the checksum is
//! verified (#52).

mod common;
use common::{dir, file, MemDev};

use fs_core::BlockRead;
use fs_erofs::{mkfs, Error, Filesystem};
use std::sync::Arc;

const SB: usize = 1024;
const CHECKSUM_AT: usize = SB + 0x04;
const FEATURE_COMPAT_AT: usize = SB + 0x08;
const BLKSZBITS_AT: usize = SB + 0x0C;
const DIRBLKBITS_AT: usize = SB + 0x5A;

fn image_with_dirblkbits(value: u8) -> Vec<u8> {
    let tree = dir(vec![
        ("a.txt", file(b"alpha\n")),
        ("sub", dir(vec![("b.txt", file(b"beta\n"))])),
    ]);
    let mut img = mkfs::build_image(tree, 12).expect("build image");
    assert_eq!(
        img[DIRBLKBITS_AT], 0,
        "the writer is expected to write zero"
    );
    img[DIRBLKBITS_AT] = value;

    let compat = u32::from_le_bytes(
        img[FEATURE_COMPAT_AT..FEATURE_COMPAT_AT + 4]
            .try_into()
            .unwrap(),
    );
    if compat & 1 != 0 {
        // CRC32C over the superblock to the end of its block, checksum
        // field zeroed, no final XOR -- `Superblock::verify_checksum`.
        let block_size = 1usize << img[BLKSZBITS_AT];
        let want_len = block_size - SB % block_size;
        let mut span = img[SB..SB + want_len].to_vec();
        span[4..8].fill(0);
        let crc = crc32c::crc32c(&span) ^ 0xFFFF_FFFF;
        img[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&crc.to_le_bytes());
    }
    img
}

fn open_bytes(img: Vec<u8>) -> Result<Filesystem, Error> {
    Filesystem::open(Arc::new(MemDev::new(img)) as Arc<dyn BlockRead>)
}

#[test]
fn a_directory_block_size_other_than_the_block_size_is_refused() {
    for value in [9u8, 13, 16] {
        match open_bytes(image_with_dirblkbits(value)) {
            Err(Error::BadSuperblock(why)) => assert!(
                why.contains("dirblkbits"),
                "dirblkbits {value} refused, but the reason {why:?} does not name it"
            ),
            Err(other) => panic!("dirblkbits {value} refused for the wrong reason: {other:?}"),
            Ok(_) => panic!(
                "dirblkbits {value} on a 4 KiB-block image opened, and read_dir would walk \
                 its directories in 4 KiB steps regardless"
            ),
        }
    }
}

/// The negative control: zero (what erofs-utils writes) and the block
/// size's own shift both mean the directory block is the filesystem
/// block, and must still open and list.
#[test]
fn zero_and_the_block_shift_still_list() {
    for value in [0u8, 12] {
        let fs = open_bytes(image_with_dirblkbits(value))
            .unwrap_or_else(|e| panic!("dirblkbits {value} must open: {e}"));
        let root = fs.root_inode().expect("root");
        let mut names: Vec<Vec<u8>> = fs
            .read_dir(&root)
            .expect("list root")
            .into_iter()
            .map(|e| e.name)
            .filter(|n| n != b"." && n != b"..")
            .collect();
        names.sort();
        assert_eq!(names, [&b"a.txt"[..], b"sub"], "dirblkbits {value}");
    }
}
