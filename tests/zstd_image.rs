//! Reading a ZSTD-compressed image, against a committed fixture and
//! against `mkfs.erofs` where it can build one.
//!
//! # Why a committed image and not only an oracle test
//!
//! `mkfs.erofs` compiles its ZSTD support in only when libzstd was
//! present at configure time, and the packaged builds on several
//! platforms are not built that way — `mkfs.erofs -V` on such a build
//! lists `lz4, lz4hc, lzma, deflate` and refuses `-zzstd` outright. An
//! oracle-only test would therefore skip on exactly the machines where
//! nobody notices it is skipping, and the codec would be effectively
//! untested.
//!
//! So `test-disks/erofs-zstd-4k.img` is a real `mkfs.erofs -b4096
//! -zzstd` image, committed, with the source tree it was built from
//! reproduced here as [`expected`]. The image is 36 KiB and it makes
//! the ZSTD path testable everywhere, with no tool required. The
//! oracle test below still runs wherever a ZSTD-capable `mkfs.erofs`
//! exists, and covers the images this one cannot: freshly built, with
//! whatever the installed writer does today.
//!
//! The fixture was verified before being committed: `fsck.erofs
//! --extract` reproduced the source tree from it byte for byte.

mod common;
use common::{mkfs_erofs_available, open_image, MemDev};

use fs_erofs::Filesystem;
use std::path::PathBuf;
use std::process::Command;

/// The generator the fixture's incompressible files were filled with.
/// Written out rather than stored, so the expectation is a statement
/// about content rather than a copy of what the driver happens to read.
fn pattern(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..len {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        v.push((x >> 16) as u8);
    }
    v
}

/// Every regular file in the fixture, and what it must contain.
///
/// The shapes are chosen so a ZSTD failure cannot hide:
///
/// - `hello.txt` is eleven bytes, so it is stored rather than
///   compressed and proves the plain path still works;
/// - `compressible.txt` is 56000 bytes of one repeated line, which
///   compresses to almost nothing — the case where one small frame
///   expands to fill a whole pcluster;
/// - `incompressible.bin` is 16384 bytes of pattern data, four
///   pclusters at this block size, so the extent map is walked and the
///   codec is called four times for one file;
/// - `sub/mixed.bin` alternates between the two, which is what makes
///   the per-pcluster frame boundaries land somewhere awkward.
fn expected() -> Vec<(&'static str, Vec<u8>)> {
    let mut mixed = pattern(9000);
    mixed.extend(std::iter::repeat_n(b'z', 8000));
    mixed.extend(pattern(3000));
    vec![
        ("/hello.txt", b"hello zstd\n".to_vec()),
        (
            "/compressible.txt",
            b"the same line over and over\n".repeat(2000),
        ),
        ("/incompressible.bin", pattern(16384)),
        ("/sub/mixed.bin", mixed),
        ("/sub/tiny.txt", b"t".to_vec()),
    ]
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-disks/erofs-zstd-4k.img")
}

fn read_whole(fs: &Filesystem, path: &str) -> Vec<u8> {
    let inode = fs
        .lookup_path(path)
        .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
    let mut out = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut out)
        .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
    out
}

#[test]
fn the_committed_zstd_image_reads_back_exactly() {
    let bytes = std::fs::read(fixture_path()).expect("read the committed zstd fixture");
    let fs = open_image(bytes);

    // The superblock says zstd, and the COMPR_CFGS record for it parsed
    // — before this codec existed, an image advertising it was refused
    // here rather than at the first read.
    let cfgs = *fs.compr_cfgs().expect("the image advertises COMPR_CFGS");
    let zstd = cfgs.zstd.expect("the blob carries a ZSTD record");
    assert_eq!(zstd.format, 0, "mkfs.erofs emits format 0");
    // `windowlog` is `ZSTD_windowLog - 10`, and the format caps the
    // dictionary at 1 MiB — so the field never exceeds 10. The parser
    // enforces that; this says what a real writer actually emits, which
    // is well under it.
    assert!(
        u32::from(zstd.windowlog) + 10 <= fs_erofs::decompress::Z_EROFS_ZSTD_MAX_DICT_LOG,
        "windowlog {} is past what the format allows",
        zstd.windowlog
    );

    for (path, want) in expected() {
        let got = read_whole(&fs, path);
        assert_eq!(
            got.len(),
            want.len(),
            "{path}: read {} bytes, expected {}",
            got.len(),
            want.len()
        );
        assert!(got == want, "{path}: contents differ");
    }

    let link = fs.lookup_path("/sub/link").expect("lookup the symlink");
    assert_eq!(
        fs.read_symlink_target(&link).expect("readlink"),
        b"../hello.txt".to_vec()
    );
}

/// Reading from an offset exercises a different arithmetic than reading
/// from zero: the pcluster the offset lands in is decoded and then
/// sliced. With one frame per pcluster, an off-by-one in that slicing
/// shows up as plausible-looking data from the wrong place, which is
/// the failure a whole-file read would not catch.
#[test]
fn partial_reads_of_a_zstd_file_land_where_they_should() {
    let bytes = std::fs::read(fixture_path()).expect("read the committed zstd fixture");
    let fs = open_image(bytes);
    let want = pattern(16384);
    let inode = fs.lookup_path("/incompressible.bin").expect("lookup");

    for (off, len) in [
        (0usize, 1usize),
        (1, 4095),
        (4095, 2),   // across the first pcluster boundary
        (4096, 100), // the start of the second
        (8191, 4098),
        (16383, 1),    // the last byte
        (0, 16384),    // the whole thing
        (12000, 4384), // to exactly EOF
    ] {
        let mut buf = vec![0u8; len];
        fs.read_file(&inode, off as u64, &mut buf)
            .unwrap_or_else(|e| panic!("read at {off}+{len}: {e:?}"));
        assert_eq!(
            buf,
            want[off..off + len],
            "read at {off}+{len} returned wrong bytes"
        );
    }

    // Past the end is refused rather than shortened, which is this
    // crate's contract everywhere else and must not become "whatever
    // the codec happened to leave in the buffer" for ZSTD.
    let mut buf = vec![0u8; 8];
    assert!(fs.read_file(&inode, 16380, &mut buf).is_err());
}

/// True when the `mkfs.erofs` on PATH was built against libzstd. It is
/// not enough for the binary to exist: ZSTD support is a compile-time
/// option and a build without it lists only `lz4, lz4hc, lzma, deflate`
/// and fails `-zzstd` with "Cannot find a valid compressor zstd".
fn mkfs_erofs_speaks_zstd() -> bool {
    if !mkfs_erofs_available() {
        return false;
    }
    let speaks = Command::new("mkfs.erofs")
        .arg("-V")
        .output()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_lowercase()
                + &String::from_utf8_lossy(&o.stderr).to_lowercase();
            text.contains("zstd")
        })
        .unwrap_or(false);
    // A CAPABILITY, BUT NOT AN OPTIONAL ONE IN CI. The workflow installs
    // `libzstd-dev` before building erofs-utils from source precisely so
    // this returns true, so a build here that cannot speak ZSTD means
    // the install step changed -- the same class of breakage as the tool
    // being absent, and worth the same loud failure rather than a line
    // on stderr nobody reads.
    //
    // The distribution's packaged erofs-utils frequently lacks it, which
    // is why the skip exists at all and why it stays for a developer.
    assert!(
        speaks || std::env::var_os("CI").is_none(),
        "the mkfs.erofs on PATH was built without libzstd, and CI is set. The workflow \
         installs libzstd-dev and builds erofs-utils from source so this comparison can \
         run; without it the ZSTD oracle would skip and the suite would pass having \
         checked the codec against nothing it did not write itself."
    );
    speaks
}

/// The same content, compressed by whatever `mkfs.erofs` is installed
/// rather than by the one that made the fixture, and under several
/// layouts rather than one.
///
/// The committed image is frozen at one writer version and one set of
/// flags. This catches what that cannot: a writer that starts emitting
/// frames differently, and the layouts where a ZSTD frame is not simply
/// one-per-block — big pclusters put many blocks behind one frame,
/// ztailpacking moves the last one inline into the inode, and fragments
/// move it into a shared packed file.
#[test]
fn a_freshly_built_zstd_image_reads_back_exactly() {
    if !mkfs_erofs_speaks_zstd() {
        eprintln!("mkfs.erofs is absent or was built without libzstd — skipping");
        return;
    }
    let files = expected();
    for flags in [
        &["-b4096", "-zzstd"][..],
        // One frame covering sixteen blocks rather than one, so a
        // pcluster's decoded span is no longer the block size.
        &["-b4096", "-zzstd", "-C65536"][..],
        // The tail pcluster inlined into the inode: the compressed
        // bytes move into the inode without the codec changing.
        &["-b4096", "-zzstd", "-Eztailpacking"][..],
        // NOT `-Efragments`. A file with both full pclusters and a
        // packed tail cannot be read at all today, for every codec,
        // and that is issue #62 rather than anything to do with ZSTD.
        // Adding it here would leave this test red for a reason it is
        // not about.
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("sub")).expect("create source tree");
        for (path, data) in &files {
            std::fs::write(src.join(path.trim_start_matches('/')), data)
                .expect("write source file");
        }
        let img = dir.path().join("out.img");
        let out = Command::new("mkfs.erofs")
            .args(flags)
            .args(["-T0", "--all-time"])
            .arg(&img)
            .arg(&src)
            .output()
            .expect("spawn mkfs.erofs");
        assert!(
            out.status.success(),
            "mkfs.erofs {flags:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let fs =
            Filesystem::open(MemDev::arc(std::fs::read(&img).expect("read image"))).expect("open");
        for (path, want) in &files {
            assert!(
                read_whole(&fs, path) == *want,
                "{path}: contents differ from what mkfs.erofs {flags:?} was given"
            );
        }
    }
}
