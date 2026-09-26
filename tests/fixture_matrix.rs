//! Every `mkfs.erofs` geometry this project claims to read, read back.
//!
//! # Why a fixture and not another oracle test
//!
//! The oracle tier runs `mkfs.erofs` in the harness VM and reads what it
//! produced. That needs KVM, and GitHub's arm64 runners have none — so
//! on aarch64, the architecture this crate ships on, nothing was reading
//! a genuine `mkfs.erofs` image at all. These fixtures are built once,
//! in the guest, by `chore fixtures`, and are then ordinary files: the
//! same bytes on every architecture, readable with no VM.
//!
//! They are also the matrix #128 asked for. One source tree, eleven
//! images: uncompressed, each of the four codecs, three block sizes, a
//! big pcluster, an inlined tail, and a chunked inode.
//!
//! # The expectation is a rule, not a recording
//!
//! [`expected`] regenerates the source tree's bytes from the same
//! description `test-disks/guest-build-images.sh` generated them from —
//! including the 16 KiB pseudo-random file, from the LCG both sides
//! spell out. An expectation copied from what the reader returned would
//! agree with the reader for ever, including when both are wrong.
//!
//! `-Efragments`, `-Eall-fragments` and `-Ededupe` are deliberately
//! absent: this crate reads those back as wrong bytes rather than as an
//! error, which is #125 and belongs in a test that names it.

use fs_erofs::Filesystem;
use fs_erofs_test_support::fixture;
use std::sync::Arc;

mod common;
use common::MemDev;

/// Every image `chore fixtures` builds, and the flags it was built with
/// (for the failure message). THIS LIST AND
/// `test-disks/guest-build-images.sh` MUST AGREE — `tests/fixtures_present`
/// below is what notices when they do not.
const IMAGES: [(&str, &str); 11] = [
    ("plain", "-b4096"),
    ("lz4", "-b4096 -zlz4"),
    ("lz4hc", "-b4096 -zlz4hc"),
    ("lzma", "-b4096 -zlzma"),
    ("deflate", "-b4096 -zdeflate"),
    ("zstd", "-b4096 -zzstd"),
    ("b1024", "-b1024 -zlz4"),
    ("b512", "-b512"),
    ("bigpcluster", "-b4096 -zlz4 -C65536"),
    ("ztailpacking", "-b4096 -zlz4 -Eztailpacking"),
    ("chunked", "-b4096 --chunksize=65536"),
];

/// The generator the fixture's pseudo-random file was filled with, and
/// the one `guest-build-images.sh` uses: a textbook LCG, so the
/// expectation is a statement about content rather than a copy of what
/// the driver happens to read.
fn lcg(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..len {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        out.push((x >> 16) as u8);
    }
    out
}

/// Every regular file in the source tree, and what it must contain.
///
/// The shapes are chosen so a layout failure cannot hide:
///
/// - `hello.txt` is twelve bytes, so it is stored inline in the inode;
/// - `empty.bin` is zero bytes, the degenerate inode;
/// - `compressible.txt` is 56000 bytes of one repeated line, which
///   compresses to almost nothing — one small frame filling a pcluster;
/// - `incompressible.bin` is 16384 bytes of LCG output, several
///   pclusters the codec cannot shrink, so the extent map is walked;
/// - `sub/deeper/leaf.bin` is 5000 bytes, a whole block plus a tail, two
///   directories down.
fn expected() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("/hello.txt", b"hello erofs\n".to_vec()),
        ("/empty.bin", Vec::new()),
        (
            "/compressible.txt",
            b"the same line over and over\n".repeat(2000),
        ),
        ("/incompressible.bin", lcg(16384)),
        ("/sub/nested.txt", b"nested\n".to_vec()),
        ("/sub/deeper/leaf.bin", vec![0x11u8; 5000]),
    ]
}

fn open_fixture(name: &str) -> Filesystem {
    let path = fixture(env!("CARGO_MANIFEST_DIR"), &format!("erofs-{name}.img"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let dev: Arc<dyn fs_core::BlockRead> = MemDev::arc(bytes);
    Filesystem::open(dev).unwrap_or_else(|e| panic!("open {path}: {e:?}"))
}

fn read_whole(fs: &Filesystem, path: &str) -> Vec<u8> {
    let inode = fs
        .lookup_path(path)
        .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
    assert!(inode.is_regular_file(), "{path} is not a regular file");
    let mut buf = vec![0u8; inode.size as usize];
    if !buf.is_empty() {
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
    }
    buf
}

#[test]
fn every_geometry_reads_back_exactly() {
    let want = expected();
    for (name, flags) in IMAGES {
        let fs = open_fixture(name);
        for (path, bytes) in &want {
            let got = read_whole(&fs, path);
            assert_eq!(
                got.len(),
                bytes.len(),
                "erofs-{name}.img ({flags}): {path} size"
            );
            assert!(
                got == *bytes,
                "erofs-{name}.img ({flags}): {path} contents differ from what \
                 mkfs.erofs was given"
            );
        }
    }
}

#[test]
fn every_geometry_resolves_its_symlink_and_directories() {
    for (name, flags) in IMAGES {
        let fs = open_fixture(name);
        let link = fs
            .lookup_path("/link")
            .unwrap_or_else(|e| panic!("erofs-{name}.img ({flags}): lookup /link: {e:?}"));
        assert!(
            link.is_symlink(),
            "erofs-{name}.img ({flags}): /link is not a symlink"
        );
        let target = fs
            .read_symlink_target(&link)
            .unwrap_or_else(|e| panic!("erofs-{name}.img ({flags}): read /link: {e:?}"));
        assert_eq!(
            target, b"hello.txt",
            "erofs-{name}.img ({flags}): symlink target"
        );

        for path in ["/sub", "/sub/deeper"] {
            let inode = fs
                .lookup_path(path)
                .unwrap_or_else(|e| panic!("erofs-{name}.img ({flags}): lookup {path}: {e:?}"));
            assert!(
                inode.is_dir(),
                "erofs-{name}.img ({flags}): {path} is not a directory"
            );
        }
    }
}

/// The block size each fixture was built with is the one the superblock
/// reports.
///
/// Reading the file back proves the reader and the writer agree; this
/// proves they agree about the number the FLAGS asked for, which is what
/// makes `b512` and `b1024` different fixtures rather than three copies
/// of the same one.
#[test]
fn the_block_size_is_the_one_the_flags_asked_for() {
    for (name, flags) in IMAGES {
        let want: u64 = match name {
            "b512" => 512,
            "b1024" => 1024,
            _ => 4096,
        };
        let fs = open_fixture(name);
        assert_eq!(
            fs.superblock().block_size(),
            want,
            "erofs-{name}.img ({flags}): block size"
        );
    }
}
