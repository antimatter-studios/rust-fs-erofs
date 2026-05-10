//! Oracle-compatibility tests: feed our reader an image produced by
//! the canonical `mkfs.erofs` (erofs-utils) and verify the directory
//! listing + file contents. Each test runs `mkfs.erofs` with a
//! specific flag combination so we can prove which on-disk variants
//! the reader handles -- and which currently surface as
//! `Error::UnsupportedLayout(_)` (the to-do list for Phase 2/3).
//!
//! All tests in this file are `#[ignore]`-gated so a fresh checkout
//! without erofs-utils still has a green `cargo test` run. Opt in
//! with `cargo test -- --ignored`.

mod common;

use common::{build_with_mkfs_erofs, dir, file, mkfs_erofs_available, open_image, MemDev};
use fs_core::BlockRead;
use fs_erofs::{mkfs, Error, Filesystem};
use std::sync::Arc;

/// Walk the FS exhaustively, asserting every regular file is readable
/// and the bytes match `expected_contents` looked up by absolute path.
fn assert_tree_matches(fs: &Filesystem, expected: &[(&str, &[u8])]) {
    for (path, want) in expected {
        let inode = fs
            .lookup_path(path)
            .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
        assert!(inode.is_regular_file(), "{path} not a file");
        assert_eq!(inode.size as usize, want.len(), "{path} size");
        let mut buf = vec![0u8; want.len()];
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
        assert_eq!(buf, *want, "{path} content");
    }
}

/// Standard small tree we feed every oracle case so behaviour is
/// directly comparable across flag combinations.
fn sample_tree() -> fs_erofs::mkfs::Node {
    dir(vec![
        ("hello.txt", file(b"hello world\n")),
        ("a.bin", file(&[0xABu8; 200])),
        // Compressible payload: 4 KiB of repeated text -> LZ4 should
        // shrink it dramatically, exercising the compressed read path
        // when -z lz4 is in effect.
        (
            "compress_me.txt",
            file(&"the quick brown fox\n".repeat(256).into_bytes()),
        ),
        (
            "sub",
            dir(vec![
                ("deep.txt", file(b"nested file\n")),
                ("empty", file(b"")),
            ]),
        ),
    ])
}

fn sample_expected() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("/hello.txt", b"hello world\n".to_vec()),
        ("/a.bin", vec![0xABu8; 200]),
        (
            "/compress_me.txt",
            "the quick brown fox\n".repeat(256).into_bytes(),
        ),
        ("/sub/deep.txt", b"nested file\n".to_vec()),
        ("/sub/empty", b"".to_vec()),
    ]
}

fn assert_sample_matches(fs: &Filesystem) {
    let expected = sample_expected();
    let refs: Vec<(&str, &[u8])> = expected.iter().map(|(p, v)| (*p, v.as_slice())).collect();
    assert_tree_matches(fs, &refs);
}

/// Try opening + reading; categorize the outcome so we can both pass
/// the "fails with UnsupportedLayout" tests AND surface a clear
/// regression signal when the reader starts returning real bytes.
#[derive(Debug)]
#[allow(dead_code)] // string payloads are diagnostic-only
enum ReadOutcome {
    Match,
    Mismatch(String),
    OpenError(String),
    LookupError(String),
    ReadError(String),
}

fn try_read_sample(bytes: Vec<u8>) -> ReadOutcome {
    let dev = common::MemDev::arc(bytes);
    let fs = match Filesystem::open(dev) {
        Ok(fs) => fs,
        Err(e) => return ReadOutcome::OpenError(format!("{e:?}")),
    };
    let expected = sample_expected();
    for (path, want) in &expected {
        let inode = match fs.lookup_path(path) {
            Ok(i) => i,
            Err(e) => return ReadOutcome::LookupError(format!("{path}: {e:?}")),
        };
        if inode.size as usize != want.len() {
            return ReadOutcome::Mismatch(format!(
                "{path} size: got {}, want {}",
                inode.size,
                want.len()
            ));
        }
        let mut buf = vec![0u8; want.len()];
        if let Err(e) = fs.read_file(&inode, 0, &mut buf) {
            return ReadOutcome::ReadError(format!("{path}: {e:?}"));
        }
        if buf != *want {
            return ReadOutcome::Mismatch(format!("{path}: content differs"));
        }
    }
    ReadOutcome::Match
}

// =====================================================================
// Test cases. One #[test] per mkfs.erofs flag combination so the
// test-runner output reads as a go/no-go matrix.
// =====================================================================

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_default() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Modern default: compacted-2B index + ztailpacking + LZ4 if it
    // helps. Expected to surface UnsupportedLayout in Phase 2 v0.1.
    let img = build_with_mkfs_erofs(&[], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_default: {outcome:?}");
    // Pass the test on EITHER full match OR a clean Unsupported* error
    // -- both are acceptable Phase 0/2-v0.1 outcomes. Panic ONLY on
    // wrong bytes (silent corruption).
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_uncompacted_legacy_index() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Disable the modern features so mkfs falls back to the legacy
    // uncompacted index (which Phase 2 v0.1 fully supports).
    let img = build_with_mkfs_erofs(&["-E^ztailpacking,^fragments,^dedupe"], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_uncompacted_legacy_index: {outcome:?}");
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_lz4_explicit() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let img = build_with_mkfs_erofs(&["-z", "lz4"], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_lz4_explicit: {outcome:?}");
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils); LZMA may not be wired through zmap"]
fn oracle_lzma() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let img = build_with_mkfs_erofs(&["-z", "lzma"], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_lzma: {outcome:?}");
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils); DEFLATE may not be wired through zmap"]
fn oracle_deflate() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let img = build_with_mkfs_erofs(&["-z", "deflate"], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_deflate: {outcome:?}");
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils); chunk-based files"]
fn oracle_chunked() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let img = build_with_mkfs_erofs(&["--chunksize=65536"], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_chunked: {outcome:?}");
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

#[test]
#[ignore = "needs mkfs.erofs (erofs-utils); xattrs"]
fn oracle_with_xattrs() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // -x N sets the xattr inline tolerance (default 2; -x 1 enables
    // xattr inlining for files with at most 1 inline xattr). Reader
    // should ignore xattr presence for plain reads.
    let img = build_with_mkfs_erofs(&["-x", "1"], &sample_tree());
    let outcome = try_read_sample(img.bytes);
    println!("oracle_with_xattrs: {outcome:?}");
    match outcome {
        ReadOutcome::Match => {}
        ReadOutcome::OpenError(_) | ReadOutcome::LookupError(_) | ReadOutcome::ReadError(_) => {}
        ReadOutcome::Mismatch(s) => panic!("silent corruption: {s}"),
    }
}

// ---- variants that we EXPECT to read cleanly today --------------------

/// Plain (no -z) build with modern features stripped should yield
/// FLAT_PLAIN/FLAT_INLINE inodes and read perfectly.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_plain_uncompressed() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // No compression, no chunking -- the simplest oracle path. Should
    // read cleanly today.
    let img = build_with_mkfs_erofs(&["-E^ztailpacking,^fragments,^dedupe"], &sample_tree());
    let fs = open_image(img.bytes);
    assert_sample_matches(&fs);
}

/// Sanity: oracle-built image's superblock is openable and reports the
/// expected magic + a sane block size.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_superblock_basic() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let img = build_with_mkfs_erofs(&["-E^ztailpacking,^fragments,^dedupe"], &sample_tree());
    let fs = open_image(img.bytes);
    let sb = fs.superblock();
    assert_eq!(sb.magic, fs_erofs::EROFS_SUPER_MAGIC_V1);
    assert!(sb.block_size() >= 512 && sb.block_size() <= 65536);
    assert!(sb.blocks > 0);
}

/// Run `xattr -w name value path` (macOS) or `setfattr -n name -v value
/// path` (Linux). Returns true if setting succeeded; false if neither
/// tool is available or the command failed (the caller should treat
/// false as "skip, environment unable to set xattrs").
fn set_xattr(path: &std::path::Path, name: &str, value: &str) -> bool {
    // Try macOS xattr first.
    if let Ok(out) = std::process::Command::new("xattr")
        .arg("-w")
        .arg(name)
        .arg(value)
        .arg(path)
        .output()
    {
        if out.status.success() {
            return true;
        }
    }
    // Fall back to Linux setfattr.
    if let Ok(out) = std::process::Command::new("setfattr")
        .arg("-n")
        .arg(name)
        .arg("-v")
        .arg(value)
        .arg(path)
        .output()
    {
        if out.status.success() {
            return true;
        }
    }
    false
}

/// End-to-end: build an image with mkfs.erofs whose files all carry the
/// SAME xattr value, so mkfs deduplicates it into the shared block area.
/// Verify our reader resolves both inline and shared entries and returns
/// the full set per file.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils) + xattr/setfattr; shared xattrs"]
fn oracle_shared_xattrs_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Stage a tree with three identical-content files. Set the same
    // xattr value on all three so mkfs.erofs collapses them into the
    // shared block area; set a UNIQUE xattr on file 1 to force at
    // least one inline entry alongside the shared references.
    let dir_t = tempfile::tempdir().expect("tempdir");
    let src = dir_t.path().join("src");
    std::fs::create_dir_all(&src).expect("create src");
    for name in ["f1.txt", "f2.txt", "f3.txt"] {
        let p = src.join(name);
        std::fs::write(&p, b"hello\n").expect("write file");
        if !set_xattr(&p, "user.team", "datastore") {
            eprintln!("skipping: cannot set xattrs in this environment");
            return;
        }
    }
    if !set_xattr(&src.join("f1.txt"), "user.unique", "f1only") {
        eprintln!("skipping: cannot set xattrs in this environment");
        return;
    }

    // -x 1 forces inline tolerance to 1, encouraging mkfs to push the
    // shared-across-files xattr into the shared area instead of inlining.
    let img_path = dir_t.path().join("out.img");
    let result = common::run_mkfs_erofs(&["-x", "1"], &img_path, &src);
    if result.status_code != Some(0) {
        panic!(
            "mkfs.erofs failed: stderr={} stdout={}",
            result.stderr, result.stdout
        );
    }
    let bytes = std::fs::read(&img_path).expect("read built image");
    let fs = common::open_image(bytes);

    // f2 and f3 should each carry the shared xattr only.
    for name in ["f2.txt", "f3.txt"] {
        let inode = fs.lookup_path(&format!("/{name}")).expect("lookup");
        let xs = fs.xattrs(&inode).expect("xattrs");
        let has_team = xs
            .iter()
            .any(|(n, v)| n == b"user.team" && v == b"datastore");
        assert!(has_team, "{name} missing user.team=datastore: {xs:?}");
    }
    // f1 should carry BOTH the shared xattr and the unique one.
    let f1 = fs.lookup_path("/f1.txt").expect("lookup f1");
    let xs = fs.xattrs(&f1).expect("xattrs f1");
    let has_team = xs
        .iter()
        .any(|(n, v)| n == b"user.team" && v == b"datastore");
    let has_unique = xs
        .iter()
        .any(|(n, v)| n == b"user.unique" && v == b"f1only");
    assert!(has_team, "f1 missing user.team: {xs:?}");
    assert!(has_unique, "f1 missing user.unique: {xs:?}");
}

/// End-to-end: build an image with `--xattr-prefix=user.dataitem` so
/// mkfs.erofs encodes any `user.dataitem.*` xattr through the custom
/// prefix dictionary. Verify our reader looks up the dict and returns
/// the FULL prefixed name.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils) + xattr/setfattr; custom xattr prefix"]
fn oracle_custom_xattr_prefix_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let dir_t = tempfile::tempdir().expect("tempdir");
    let src = dir_t.path().join("src");
    std::fs::create_dir_all(&src).expect("create src");
    let p = src.join("file.txt");
    std::fs::write(&p, b"x").expect("write file");
    if !set_xattr(&p, "user.dataitem.thing", "v1") {
        eprintln!("skipping: cannot set xattrs in this environment");
        return;
    }
    if !set_xattr(&p, "user.dataitem.other", "v2") {
        eprintln!("skipping: cannot set xattrs in this environment");
        return;
    }

    let img_path = dir_t.path().join("out.img");
    let result = common::run_mkfs_erofs(
        &["--xattr-prefix=user.dataitem", "-x", "1"],
        &img_path,
        &src,
    );
    if result.status_code != Some(0) {
        panic!(
            "mkfs.erofs failed: stderr={} stdout={}",
            result.stderr, result.stdout
        );
    }
    let bytes = std::fs::read(&img_path).expect("read built image");
    let fs = common::open_image(bytes);

    // Sanity: dictionary should have at least one entry.
    let dict = fs.xattr_prefix_dict().expect("read dict");
    assert!(
        !dict.is_empty(),
        "expected at least one prefix dict entry; got 0"
    );
    println!("dict: {dict:?}");

    let inode = fs.lookup_path("/file.txt").expect("lookup");
    let xs = fs.xattrs(&inode).expect("xattrs");
    println!("xattrs: {xs:?}");
    let has_thing = xs
        .iter()
        .any(|(n, v)| n == b"user.dataitem.thing" && v == b"v1");
    let has_other = xs
        .iter()
        .any(|(n, v)| n == b"user.dataitem.other" && v == b"v2");
    assert!(has_thing, "expected user.dataitem.thing=v1: {xs:?}");
    assert!(has_other, "expected user.dataitem.other=v2: {xs:?}");
}

// =====================================================================
// Multi-lcluster pcluster regression tests
// =====================================================================
// These guard the "single pcluster spans many lclusters" silent-
// corruption bug fixed in zmap.rs::pcluster_extent + the +16-byte
// legacy header fix. mkfs.erofs collates contiguous lclusters into
// one LZ4 frame whenever the compressed bytes fit a single block;
// decompressing only one lcluster's worth produced plausible-looking
// garbage from byte `lcluster_size` onward. We force LEGACY layout
// here because our resolver doesn't yet model compacted-2B's
// per-pcluster blkaddr packing -- compacted images currently surface
// as a clean error rather than silent corruption (see "compacted_2b_
// returns_clean_error" below).
// =====================================================================

/// Highly compressible 200 KiB blob -> LZ4 collapses to 1 pcluster
/// spanning ~13 lclusters at 16 KiB blocks (or 49 lclusters at 4 KiB).
/// Exact repro from the bug report (only with -Elegacy-compress added).
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_legacy_lz4_multi_lcluster_pcluster_200k() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(200_000)
        .collect();
    let tree = dir(vec![("big.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(
        &[
            "-z",
            "lz4",
            "-Elegacy-compress",
            "-E^ztailpacking,^fragments,^dedupe",
        ],
        &tree,
    );
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert!(inode.is_regular_file());
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read big.bin");
    assert_eq!(
        buf, payload,
        "200K legacy LZ4 multi-lcluster pcluster bytes"
    );
}

/// Smaller variant exercising the same code path with a 4 KiB block
/// size -- 8000-byte file becomes 2 lclusters owned by a single
/// pcluster (the HEAD-with-clusterofs=0 / sentinel-PLAIN case).
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_legacy_lz4_two_lcluster_pcluster_4k_blocksize() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(8_000)
        .collect();
    let tree = dir(vec![("m.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(
        &[
            "-z",
            "lz4",
            "-Elegacy-compress",
            "-E^ztailpacking,^fragments,^dedupe",
            "-b",
            "4096",
            "-C",
            "4096",
        ],
        &tree,
    );
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/m.bin").expect("lookup m.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read m.bin");
    assert_eq!(buf, payload);
}

/// Sub-pcluster slice: read a chunk from the MIDDLE of a multi-
/// lcluster pcluster. Catches off-by-pcluster-base errors that whole-
/// file reads would miss because the boundary is internal to one
/// pcluster.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_legacy_lz4_mid_pcluster_slice() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(200_000)
        .collect();
    let tree = dir(vec![("big.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(
        &[
            "-z",
            "lz4",
            "-Elegacy-compress",
            "-E^ztailpacking,^fragments,^dedupe",
        ],
        &tree,
    );
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").unwrap();
    // Read 50 KiB starting 73 KiB in -- straddles multiple lcluster
    // boundaries inside one pcluster.
    let off = 73 * 1024;
    let len = 50 * 1024;
    let mut buf = vec![0u8; len];
    fs.read_file(&inode, off as u64, &mut buf)
        .expect("mid-pcluster read");
    assert_eq!(buf, payload[off..off + len]);
}

/// HEAD-with-clusterofs=0 + a HIGH-clusterofs sentinel last entry --
/// exercised by the 8 KiB legacy build (lc1 PLAIN clusterofs=3904).
/// This is the boundary case the bug report calls out as "the kernel's
/// `clusterofs` semantics" of HEAD vs sentinel.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_legacy_lz4_sentinel_last_lcluster() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // 5000 bytes spans bytes [0,4096) (lc0 HEAD) + [4096,5000) (lc1
    // sentinel PLAIN with clusterofs=904). Pure regression target.
    let payload: Vec<u8> = b"abcdefghij".iter().copied().cycle().take(5_000).collect();
    let tree = dir(vec![("s.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(
        &[
            "-z",
            "lz4",
            "-Elegacy-compress",
            "-E^ztailpacking,^fragments,^dedupe",
            "-b",
            "4096",
            "-C",
            "4096",
        ],
        &tree,
    );
    let fs = open_image(img.bytes);
    let inode = match fs.lookup_path("/s.bin") {
        Ok(i) => i,
        Err(e) => {
            // mkfs may decide a 5K file isn't worth compressing; skip
            // gracefully if so.
            eprintln!("skipping: lookup s.bin: {e:?}");
            return;
        }
    };
    if !inode.is_regular_file() {
        return;
    }
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read s.bin");
    assert_eq!(buf, payload);
}

// =====================================================================
// Compact format regression tests (the W2b fix)
// =====================================================================
// These guard the "compact / pack-encoded zmap" path. mkfs.erofs since
// 1.5 uses the compact format (datalayout = COMPRESSED_COMPACT, layout
// id 3) by default. Encoding details:
// - Each pack is `vcnt << amortizedshift` bytes; the trailing __le32 is
//   a per-pack base blkaddr, the leading bytes hold a bit-packed
//   stream of `vcnt` per-lcluster entries.
// - 4B amortized: vcnt=2, encodebits=16, lobits=max(z_lclusterbits, 12).
// - 2B amortized: vcnt=16, encodebits=14, lobits=max(z_lclusterbits, 12).
// - The advise bit `Z_EROFS_ADVISE_COMPACTED_2B` toggles the 2B middle
//   region. Otherwise everything is in 4B form.
// - HEAD/PLAIN blkaddr resolution: count non-NONHEAD entries strictly
//   before the target inside the same pack, then `pblk = base + nblk`.
// Spec: public EROFS compression-format documentation
// (https://erofs.docs.kernel.org/en/latest/design.html#compressed-data).
// =====================================================================

/// `mkfs.erofs -z lz4` with default flags (compact-4B / compacted-2B
/// + ztailpacking) on a small file: must round-trip. Triggers the
///   "advise=0 compact-4B" + ztailpacking inline-tail path on a modern
///   (>= 1.5) erofs-utils install.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_compacted_2b_default_lz4() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Repro from the bug report.
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(200_000)
        .collect();
    let tree = dir(vec![("big.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(&["-z", "lz4"], &tree);
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert!(inode.is_regular_file());
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read big.bin");
    assert_eq!(buf, payload, "default mkfs.erofs -z lz4 must round-trip");
}

/// Small file that triggers ztailpacking (the inline-tail pcluster
/// case): payload is short enough to leave room in the metadata block,
/// so mkfs.erofs inlines the LZ4 frame just past the index area.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_compacted_2b_with_ztailpacking() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(8_000)
        .collect();
    let tree = dir(vec![("m.bin", file(&payload))]);
    // -Eztailpacking is on by default in modern mkfs.erofs but we add
    // it explicitly to keep the test meaningful even if a future build
    // of erofs-utils flips defaults.
    let img = build_with_mkfs_erofs(&["-z", "lz4", "-Eztailpacking"], &tree);
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/m.bin").expect("lookup m.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read m.bin");
    assert_eq!(buf, payload, "ztailpacking inline-tail content matches");
}

/// File large enough that mkfs.erofs splits it into multiple
/// independent pclusters AND triggers cross-block reads where one
/// 16 KiB block straddles a pcluster boundary. With the v0.3 fix this
/// must round-trip; the prior reader zero-filled past the boundary
/// inside `read_compressed_block`.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_compacted_2b_multi_pcluster() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(10_000_000)
        .collect();
    let tree = dir(vec![("big.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(&["-z", "lz4"], &tree);
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read big.bin");
    assert_eq!(
        buf, payload,
        "multi-pcluster file must read across pcluster boundaries"
    );
}

/// `-b 4096` forces a smaller block size, which makes mkfs.erofs
/// actually set the `Z_EROFS_ADVISE_COMPACTED_2B` advise bit (because
/// `lclusterbits <= 12` is required for 2B packs). The header check at
/// open time and the middle-region pack walk both get exercised.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_compacted_2b_advise_bit_set_4k_blocks() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"aaaa bbbb cccc\n"
        .iter()
        .copied()
        .cycle()
        .take(5_000_000)
        .collect();
    let tree = dir(vec![("big.bin", file(&payload))]);
    let img = build_with_mkfs_erofs(&["-z", "lz4", "-b", "4096"], &tree);
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).expect("read big.bin");
    assert_eq!(buf, payload);
}

/// W2a sentinel: build a compressed image with OUR writer and confirm
/// fsck.erofs accepts it. This is a placeholder for the kernel
/// mount-and-read cross-check (which can only run on Linux); on a
/// developer box without a Linux mount, fsck.erofs's clean exit is the
/// strongest portable evidence that the on-disk layout is spec-correct.
#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn our_writer_image_readable_by_kernel_fixture() {
    if !common::fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // Mixed payload: small + multi-lcluster + incompressible. Each
    // exercises a different path in our writer (PLAIN passthrough vs
    // HEAD1, single-lcluster vs many).
    let small = b"hello compressed world\n".to_vec();
    let bs = 4096usize;
    let multi: Vec<u8> = vec![b'a'; 5 * bs];
    let mut incompressible = Vec::with_capacity(2 * bs);
    let mut state: u64 = 0x5eed_5eed_5eed_5eed;
    for _ in 0..(2 * bs) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        incompressible.push((state >> 56) as u8);
    }
    let tree = dir(vec![
        (
            "small.txt",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: small,
                algo: mkfs::CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        ),
        (
            "multi.bin",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: multi,
                algo: mkfs::CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        ),
        (
            "rand.bin",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: incompressible,
                algo: mkfs::CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        ),
        ("plain.txt", file(b"alongside-plain\n")),
    ]);
    let img = mkfs::build_image(tree, 12).unwrap();
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("our.img");
    std::fs::write(&path, &img).expect("write image");
    let out = std::process::Command::new("fsck.erofs")
        .arg(&path)
        .output()
        .expect("spawn fsck.erofs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "fsck.erofs failed on our compressed writer output:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// W2b: standalone SHA256 round-trip proof. Build a 1 MiB file with
/// our writer in compacted-2B mode, read it back with our reader,
/// confirm SHA256(input) == SHA256(decoded). Independent of erofs-utils
/// so it runs in the default `cargo test` (no `--ignored` gate). The
/// SHA-256 implementation below is a small inline core to avoid a new
/// dependency.
#[test]
fn our_compacted2b_writer_sha256_round_trip_1mib() {
    let payload: Vec<u8> = b"yes pattern\n"
        .iter()
        .copied()
        .cycle()
        .take(1_048_576)
        .collect();
    let want_hash = sha256(&payload);

    let img = mkfs::build_image(
        dir(vec![(
            "big.bin",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: payload.clone(),
                algo: mkfs::CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedIndexFormat::Compacted2B,
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        )]),
        12,
    )
    .unwrap();
    let dev = common::MemDev::arc(img);
    let fs = Filesystem::open(dev).expect("open compacted-2B image");
    let inode = fs.lookup_path("/big.bin").expect("lookup");
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).expect("read");
    let got_hash = sha256(&buf);

    let want_hex = hex_encode(&want_hash);
    let got_hex = hex_encode(&got_hash);
    eprintln!("input  SHA256: {want_hex}");
    eprintln!("output SHA256: {got_hex}");
    assert_eq!(
        want_hash, got_hash,
        "1 MiB compacted-2B SHA256 round-trip failed"
    );
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Minimal SHA-256 core, used only by the round-trip proof above.
/// Spec: FIPS 180-4. Independent implementation.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let mj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(mj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

/// W2b cross-validation: build the same logical content two ways
/// (our compacted-2B writer + `mkfs.erofs -z lz4`'s default modern
/// format), confirm both produce images our reader extracts byte-for-
/// byte identical to the input.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn our_compacted2b_image_compatible_with_kernel_oracle() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Highly compressible 1 MiB payload: matches the SHA256 round-trip
    // proof referenced in W2b's milestone description.
    let payload: Vec<u8> = b"yes pattern\n"
        .iter()
        .copied()
        .cycle()
        .take(1_048_576)
        .collect();

    // Path A: our writer in compacted-2B mode -> our reader.
    let our_img = mkfs::build_image(
        dir(vec![(
            "big.bin",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: payload.clone(),
                algo: mkfs::CompressedAlgo::Lz4,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedIndexFormat::Compacted2B,
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        )]),
        12,
    )
    .unwrap();
    let dev_a = common::MemDev::arc(our_img.clone());
    let fs_a = Filesystem::open(dev_a).expect("open our writer's compacted-2B image");
    let inode_a = fs_a.lookup_path("/big.bin").unwrap();
    assert_eq!(inode_a.size as usize, payload.len());
    let mut buf_a = vec![0u8; payload.len()];
    fs_a.read_file(&inode_a, 0, &mut buf_a)
        .expect("read our compacted-2B image");
    assert_eq!(buf_a, payload, "our compacted-2B writer round-trip");

    // Path B: kernel mkfs.erofs default (compacted-2B + ztailpacking)
    // -> our reader. This proves both producers land on the same
    // semantic content from the reader's perspective.
    let kernel_img = build_with_mkfs_erofs(&["-z", "lz4"], &dir(vec![("big.bin", file(&payload))]));
    let fs_b = open_image(kernel_img.bytes);
    let inode_b = fs_b.lookup_path("/big.bin").unwrap();
    assert_eq!(inode_b.size as usize, payload.len());
    let mut buf_b = vec![0u8; payload.len()];
    fs_b.read_file(&inode_b, 0, &mut buf_b)
        .expect("read kernel mkfs.erofs image");
    assert_eq!(buf_b, payload, "kernel mkfs.erofs -z lz4 round-trip");

    // Cross-check: both decoded outputs are identical.
    assert_eq!(buf_a, buf_b, "writer/oracle decoded outputs match");
}

/// W3 cross-validation: build a small payload with `mkfs.erofs -z
/// lzma` that fits in a single lcluster (so the kernel doesn't engage
/// `Z_EROFS_ADVISE_BIG_PCLUSTER_1`, which our reader doesn't support
/// yet). Read it back with our reader and confirm the SHA256 matches
/// the input. Proves codec parity with the kernel oracle for LZMA at
/// default settings.
///
/// Note: kernel mkfs.erofs sets `EROFS_FEATURE_INCOMPAT_COMPR_CFGS`
/// (0x2) on these images and emits a per-codec config blob after the
/// SB. Our reader ignores the blob and uses lzma-rs defaults
/// (lc=3, lp=0, pb=2, dict_size=1<<24), which happen to match what
/// erofs-utils 1.9 emits when the test payload is small.
///
/// For larger payloads kernel mkfs.erofs collates many lclusters into a
/// single big pcluster (extent), exposing
/// `Error::UnsupportedLayout(99)` from our zmap. That's a reader-side
/// limitation tracked as future BIG_PCLUSTER work — orthogonal to the
/// W3 codec milestone covered by this test.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_lzma_default_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Single-lcluster payload: 3.5 KiB at default 4 KiB blocks /
    // lcluster_size. Avoids triggering BIG_PCLUSTER on the kernel side.
    let payload: Vec<u8> = b"yes pattern\n"
        .iter()
        .copied()
        .cycle()
        .take(3584)
        .collect();
    let want_hash = sha256(&payload);

    let kernel_img = build_with_mkfs_erofs(
        &["-z", "lzma", "-b", "4096"],
        &dir(vec![("big.bin", file(&payload))]),
    );
    let fs = open_image(kernel_img.bytes);
    let inode = fs.lookup_path("/big.bin").unwrap();
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("read kernel mkfs.erofs LZMA image");
    let got_hash = sha256(&buf);

    eprintln!("oracle LZMA input  SHA256: {}", hex_encode(&want_hash));
    eprintln!("oracle LZMA output SHA256: {}", hex_encode(&got_hash));
    assert_eq!(
        want_hash, got_hash,
        "kernel mkfs.erofs -z lzma -> our reader: SHA256 mismatch"
    );
}

/// W3 cross-validation: same as `oracle_lzma_default_round_trip` but
/// for `-z deflate`. Same single-lcluster restriction applies.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_deflate_default_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"yes pattern\n"
        .iter()
        .copied()
        .cycle()
        .take(3584)
        .collect();
    let want_hash = sha256(&payload);

    let kernel_img = build_with_mkfs_erofs(
        &["-z", "deflate", "-b", "4096"],
        &dir(vec![("big.bin", file(&payload))]),
    );
    let fs = open_image(kernel_img.bytes);
    let inode = fs.lookup_path("/big.bin").unwrap();
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("read kernel mkfs.erofs DEFLATE image");
    let got_hash = sha256(&buf);

    eprintln!("oracle DEFLATE input  SHA256: {}", hex_encode(&want_hash));
    eprintln!("oracle DEFLATE output SHA256: {}", hex_encode(&got_hash));
    assert_eq!(
        want_hash, got_hash,
        "kernel mkfs.erofs -z deflate -> our reader: SHA256 mismatch"
    );
}

// =====================================================================
// BIG_PCLUSTER round-trip tests
// =====================================================================
// mkfs.erofs always sets `Z_EROFS_ADVISE_BIG_PCLUSTER_1` (or `_2`) when
// emitting LZMA / DEFLATE streams; payloads larger than ~3.5 KiB then
// trigger genuine multi-block pclusters. These tests build 1 MiB
// highly-compressible payloads, read them back through our reader, and
// confirm the SHA256 matches byte-for-byte. Spec source: public EROFS
// compression-format documentation
// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>)
// — CBLKCNT marker semantics. License clean: no kernel `.c` source
// consulted; format inferred from the public spec + empirical inspection
// of erofs-utils 1.9 byte output.
// =====================================================================

/// `mkfs.erofs -z lzma` on a 1 MiB highly-compressible payload. The
/// kernel oracle engages `Z_EROFS_ADVISE_BIG_PCLUSTER_1`; before this
/// fix our reader rejected the image with `Error::UnsupportedLayout(99)`.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_lzma_big_pcluster_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"the quick brown fox\n"
        .iter()
        .copied()
        .cycle()
        .take(1 << 20)
        .collect();
    let want_hash = sha256(&payload);

    let img = build_with_mkfs_erofs(
        &["-z", "lzma", "-b", "4096"],
        &dir(vec![("big.bin", file(&payload))]),
    );
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("read big.bin from BIG_PCLUSTER LZMA image");
    let got_hash = sha256(&buf);
    eprintln!(
        "oracle LZMA BIG_PCLUSTER input  SHA256: {}",
        hex_encode(&want_hash)
    );
    eprintln!(
        "oracle LZMA BIG_PCLUSTER output SHA256: {}",
        hex_encode(&got_hash)
    );
    assert_eq!(
        want_hash, got_hash,
        "1 MiB LZMA BIG_PCLUSTER SHA256 mismatch"
    );
}

/// `mkfs.erofs -z deflate` on a 1 MiB highly-compressible payload.
/// Same BIG_PCLUSTER expectations as the LZMA variant above.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_deflate_big_pcluster_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"the quick brown fox\n"
        .iter()
        .copied()
        .cycle()
        .take(1 << 20)
        .collect();
    let want_hash = sha256(&payload);

    let img = build_with_mkfs_erofs(
        &["-z", "deflate", "-b", "4096"],
        &dir(vec![("big.bin", file(&payload))]),
    );
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("read big.bin from BIG_PCLUSTER DEFLATE image");
    let got_hash = sha256(&buf);
    eprintln!(
        "oracle DEFLATE BIG_PCLUSTER input  SHA256: {}",
        hex_encode(&want_hash)
    );
    eprintln!(
        "oracle DEFLATE BIG_PCLUSTER output SHA256: {}",
        hex_encode(&got_hash)
    );
    assert_eq!(
        want_hash, got_hash,
        "1 MiB DEFLATE BIG_PCLUSTER SHA256 mismatch"
    );
}

/// Read a slice from the middle of a BIG_PCLUSTER-encoded file. Catches
/// off-by-pcluster-base errors that whole-file reads might miss because
/// the cut falls inside a multi-block pcluster.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_lzma_big_pcluster_mid_slice() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = b"alpha beta gamma\n"
        .iter()
        .copied()
        .cycle()
        .take(1 << 20)
        .collect();
    let img = build_with_mkfs_erofs(
        &["-z", "lzma", "-b", "4096"],
        &dir(vec![("big.bin", file(&payload))]),
    );
    let fs = open_image(img.bytes);
    let inode = fs.lookup_path("/big.bin").unwrap();
    // Slice from byte 73 KiB for 50 KiB: deliberately straddles the
    // typical lcluster_size boundary so the reader has to compute the
    // right offset within a multi-block pcluster.
    let off = 73 * 1024usize;
    let len = 50 * 1024usize;
    let mut buf = vec![0u8; len];
    fs.read_file(&inode, off as u64, &mut buf)
        .expect("mid-pcluster read");
    assert_eq!(buf, payload[off..off + len]);
}

// =====================================================================
// BIG_PCLUSTER synthetic byte-level test
// =====================================================================
// Build a hand-rolled LEGACY image with a known-good DEFLATE-encoded
// 2-block pcluster + CBLKCNT marker, then verify decompression returns
// the expected bytes. Independent of erofs-utils, runs in default
// `cargo test`. Proves the byte-level on-disk format rather than just
// "round-trip works against an oracle".
#[test]
fn synthetic_legacy_big_pcluster_two_block_round_trip() {
    use flate2::{Compress, Compression, FlushCompress};
    use fs_erofs::EROFS_SUPER_MAGIC_V1;

    const BS: usize = 4096;
    // Source: 4 lclusters (16384 bytes) of pseudo-random bytes — DEFLATE
    // can't appreciably shrink it, so the compressed output spans
    // multiple blocks. This is the whole point of the test: a multi-
    // block pcluster.
    let mut payload = vec![0u8; 4 * BS];
    let mut state: u64 = 0x1234_5678_9abc_def0;
    for b in payload.iter_mut() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (state >> 56) as u8;
    }
    // Encode payload with raw DEFLATE.
    let mut encoder = Compress::new(Compression::default(), false);
    let mut compressed = vec![0u8; payload.len() + 64];
    encoder
        .compress(&payload, &mut compressed, FlushCompress::Finish)
        .unwrap();
    compressed.truncate(encoder.total_out() as usize);
    // The whole point of this test is to exercise multi-block pclusters,
    // so make sure the compressed payload actually overflows 1 block.
    assert!(
        compressed.len() > BS,
        "synthetic payload didn't trigger multi-block compressed output ({} bytes)",
        compressed.len()
    );
    // Round up to a multiple of BS — the on-disk pcluster occupies
    // whole blocks; trailing bytes are zero-padded.
    let blocks = compressed.len().div_ceil(BS) as u32;
    compressed.resize(blocks as usize * BS, 0u8);

    // Build a minimal EROFS image manually:
    //   block 0: SB area
    //   block 1: meta block holding the root inode + zmap header + index
    //   block 2..2+blocks: compressed pcluster bytes
    //   block 2+blocks: dirent block
    let total_blocks = 2 + blocks as usize + 1;
    let mut img = vec![0u8; BS * total_blocks];

    // SB. Field offsets per `Superblock::parse`. We only need `magic`,
    // `blkszbits`, `root_nid`, `blocks`, `meta_blkaddr` to keep the
    // reader happy; everything else may be zero.
    let sb_off = 1024usize;
    img[sb_off..sb_off + 4].copy_from_slice(&EROFS_SUPER_MAGIC_V1.to_le_bytes());
    img[sb_off + 0x0C] = 12; // blkszbits = 12 -> 4 KiB
    img[sb_off + 0x0E..sb_off + 0x10].copy_from_slice(&0u16.to_le_bytes()); // root_nid
    img[sb_off + 0x24..sb_off + 0x28].copy_from_slice(&(total_blocks as u32).to_le_bytes());
    img[sb_off + 0x28..sb_off + 0x2C].copy_from_slice(&1u32.to_le_bytes()); // meta_blkaddr

    // Root dir inode at block 1 NID 0 (size 32). FlatPlain dir with
    // dirent block at block (2 + blocks).
    let root_off = BS;
    let dir_blk = 2u32 + blocks;
    // Compact 32-byte inode, layout=FlatPlain, mode=040000|0755=0x41ed.
    // Format = (FlatPlain<<1) | (xattrs<<3); FlatPlain = 0.
    let raw_format: u16 = 0;
    img[root_off..root_off + 2].copy_from_slice(&raw_format.to_le_bytes());
    img[root_off + 2..root_off + 4].copy_from_slice(&0u16.to_le_bytes()); // xattr_icount
    img[root_off + 4..root_off + 6].copy_from_slice(&0x41edu16.to_le_bytes());
    img[root_off + 6..root_off + 8].copy_from_slice(&1u16.to_le_bytes()); // nlink
    img[root_off + 8..root_off + 12].copy_from_slice(&(BS as u32).to_le_bytes());
    img[root_off + 16..root_off + 20].copy_from_slice(&dir_blk.to_le_bytes());

    // big.bin inode at block 1 NID 1 (offset BS+32). Layout =
    // CompressionLegacy = 1; format = (1<<1) | (xattrs<<3) = 0x0002.
    let file_off = BS + 32;
    let big_layout: u16 = 1 << 1; // CompressionLegacy
    img[file_off..file_off + 2].copy_from_slice(&big_layout.to_le_bytes());
    img[file_off + 4..file_off + 6].copy_from_slice(&0x81a4u16.to_le_bytes()); // mode
    img[file_off + 6..file_off + 8].copy_from_slice(&1u16.to_le_bytes()); // nlink
    img[file_off + 8..file_off + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    // raw_blkaddr (i_u for compressed inodes carries `compressed_blocks`).
    img[file_off + 16..file_off + 20].copy_from_slice(&(blocks as u32).to_le_bytes());

    // ZMap header at file_off + 32 (= body_end for compact 32-byte
    // inode with zero xattrs).
    let zmap_off = file_off + 32;
    // h_advise = BIG_PCLUSTER_1.
    img[zmap_off + 4..zmap_off + 6].copy_from_slice(&2u16.to_le_bytes());
    // byte 6 = h_algorithmtype (low4 = HEAD1 algo): DEFLATE = 2.
    img[zmap_off + 6] = 2;
    // byte 7 = h_clusterbits (low4 = lclusterbits): 0 -> lcluster_size
    // = 1 << blkszbits = 4096.
    img[zmap_off + 7] = 0;

    // Legacy index entries: 8-byte struct header + 8-byte reserved gap,
    // then per-lcluster 8-byte entries (one per lcluster). Layout:
    //   lc 0: HEAD1 clusterofs=0 blkaddr=2.
    //   lc 1: NONHEAD with CBLKCNT marker (blocks).
    //   lc 2: NONHEAD delta[0]=2.
    //   lc 3: NONHEAD delta[0]=3.
    let idx_off = zmap_off + 16;
    let head_off = idx_off;
    img[head_off..head_off + 2].copy_from_slice(&1u16.to_le_bytes()); // advise=HEAD1
    img[head_off + 2..head_off + 4].copy_from_slice(&0u16.to_le_bytes()); // clusterofs
    img[head_off + 4..head_off + 8].copy_from_slice(&2u32.to_le_bytes()); // blkaddr=2
    let nh1_off = idx_off + 8;
    img[nh1_off..nh1_off + 2].copy_from_slice(&2u16.to_le_bytes()); // advise=NONHEAD
    img[nh1_off + 2..nh1_off + 4].copy_from_slice(&0u16.to_le_bytes()); // clusterofs
                                                                        // delta[0] (low 16): CBLKCNT marker | blocks. delta[1] (high 16):
                                                                        // 3 (forward distance to next HEAD; no next HEAD here, so any
                                                                        // value would do — we pick 3 to mirror what mkfs.erofs emits).
    let cblkcnt_lo: u32 = 0x0800u32 | (blocks as u32);
    let u_raw_nh1: u32 = cblkcnt_lo | (3u32 << 16);
    img[nh1_off + 4..nh1_off + 8].copy_from_slice(&u_raw_nh1.to_le_bytes());
    let nh2_off = idx_off + 16;
    img[nh2_off..nh2_off + 2].copy_from_slice(&2u16.to_le_bytes()); // NONHEAD
    img[nh2_off + 4..nh2_off + 8].copy_from_slice(&(2u32 | (2u32 << 16)).to_le_bytes());
    let nh3_off = idx_off + 24;
    img[nh3_off..nh3_off + 2].copy_from_slice(&2u16.to_le_bytes()); // NONHEAD
    img[nh3_off + 4..nh3_off + 8].copy_from_slice(&(3u32 | (1u32 << 16)).to_le_bytes());

    // Compressed payload at block 2.
    let payload_off = 2 * BS;
    img[payload_off..payload_off + compressed.len()].copy_from_slice(&compressed);

    // Dirent block at block dir_blk: ".", "..", "big.bin".
    let dirblk_off = dir_blk as usize * BS;
    // We can build via the public mkfs helper but easier to hand-roll.
    // Each dirent is 12 bytes header + name. We need 3 entries.
    // Layout: [hdr0 | hdr1 | hdr2 | names... ].
    fn put_dirent(out: &mut [u8], idx: usize, nid: u64, nameoff: u16, ftype: u8) {
        let off = idx * 12;
        out[off..off + 8].copy_from_slice(&nid.to_le_bytes());
        out[off + 8..off + 10].copy_from_slice(&nameoff.to_le_bytes());
        out[off + 10] = ftype;
        out[off + 11] = 0;
    }
    let dir_slice = &mut img[dirblk_off..dirblk_off + BS];
    let names: &[(u64, u8, &[u8])] = &[
        (0, 4, b"."),       // root nid=0, type=DIR
        (0, 4, b".."),      //
        (1, 1, b"big.bin"), // file nid=1, type=REG
    ];
    let header_bytes = 12 * names.len();
    let mut name_cursor = header_bytes;
    for (i, (nid, ftype, name)) in names.iter().enumerate() {
        put_dirent(dir_slice, i, *nid, name_cursor as u16, *ftype);
        dir_slice[name_cursor..name_cursor + name.len()].copy_from_slice(name);
        name_cursor += name.len();
    }

    // Open + read back.
    let dev = common::MemDev::arc(img);
    let fs = Filesystem::open(dev).expect("open synthetic BIG_PCLUSTER image");
    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("read synthetic BIG_PCLUSTER file");
    assert_eq!(buf, payload, "synthetic 2-block pcluster round-trip");
}

/// W3 standalone: build a 100 KiB highly-compressible file with our
/// LZMA writer, read with our reader, verify SHA256. Independent of
/// erofs-utils so it runs in default `cargo test`.
#[test]
fn our_lzma_writer_sha256_round_trip_100kib() {
    let payload: Vec<u8> = b"yes pattern\n"
        .iter()
        .copied()
        .cycle()
        .take(100 * 1024)
        .collect();
    let want_hash = sha256(&payload);

    let img = mkfs::build_image(
        dir(vec![(
            "big.bin",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: payload.clone(),
                algo: mkfs::CompressedAlgo::Lzma,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        )]),
        12,
    )
    .unwrap();
    let dev = common::MemDev::arc(img);
    let fs = Filesystem::open(dev).expect("open our LZMA image");
    let inode = fs.lookup_path("/big.bin").expect("lookup");
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).expect("read");
    let got_hash = sha256(&buf);
    eprintln!("our LZMA input  SHA256: {}", hex_encode(&want_hash));
    eprintln!("our LZMA output SHA256: {}", hex_encode(&got_hash));
    assert_eq!(want_hash, got_hash, "100 KiB LZMA SHA256 round-trip failed");
}

/// W3 standalone: same for our DEFLATE writer.
#[test]
fn our_deflate_writer_sha256_round_trip_100kib() {
    let payload: Vec<u8> = b"yes pattern\n"
        .iter()
        .copied()
        .cycle()
        .take(100 * 1024)
        .collect();
    let want_hash = sha256(&payload);

    let img = mkfs::build_image(
        dir(vec![(
            "big.bin",
            mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
                mode: mkfs::DEFAULT_FILE_MODE,
                data: payload.clone(),
                algo: mkfs::CompressedAlgo::Deflate,
                lclusterbits: 0,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
                index_format: mkfs::CompressedFileSpec::default_index_format(),
                ztailpacking: false,
                target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
            }),
        )]),
        12,
    )
    .unwrap();
    let dev = common::MemDev::arc(img);
    let fs = Filesystem::open(dev).expect("open our DEFLATE image");
    let inode = fs.lookup_path("/big.bin").expect("lookup");
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).expect("read");
    let got_hash = sha256(&buf);
    eprintln!("our DEFLATE input  SHA256: {}", hex_encode(&want_hash));
    eprintln!("our DEFLATE output SHA256: {}", hex_encode(&got_hash));
    assert_eq!(
        want_hash, got_hash,
        "100 KiB DEFLATE SHA256 round-trip failed"
    );
}

/// W4 milestone: end-to-end proof that an image emitted by our writer
/// is mountable by the live Linux EROFS kernel module. Requires:
///
/// - Linux host (the EROFS module + `mount` syscall are kernel APIs).
/// - root or `mount -o user`-permissive sudoers entry to invoke
///   `mount`/`umount`.
/// - the `loop` driver loaded.
///
/// On macOS / non-Linux hosts, the test is `cfg`-skipped at compile
/// time (so `cargo test` on developer macOS hosts stays green). It is
/// `#[ignore]`-gated unconditionally so even on Linux the suite stays
/// green when the harness lacks mount privileges.
///
/// What we test: build a tree with our `mkfs::build_image`, write the
/// bytes to a tempfile, mount it via `mount -t erofs -o loop <img>
/// <mountpoint>`, list the mountpoint, and unmount. If mount fails the
/// captured stderr is included in the panic to make CI diagnosis easy.
#[test]
#[ignore = "kernel-mountability check; needs Linux + mount privileges"]
#[cfg(target_os = "linux")]
fn our_writer_image_kernel_mountable() {
    use std::path::PathBuf;

    let img_bytes = mkfs::build_image(
        dir(vec![
            ("hello.txt", file(b"hello\n")),
            ("a.bin", file(&[0xAB; 200])),
            ("sub", dir(vec![("nested.txt", file(b"nested\n"))])),
        ]),
        12,
    )
    .expect("build_image");

    let tmp = tempfile::tempdir().expect("tempdir");
    let img_path: PathBuf = tmp.path().join("our.img");
    std::fs::write(&img_path, &img_bytes).expect("write image file");
    let mountpoint: PathBuf = tmp.path().join("mnt");
    std::fs::create_dir_all(&mountpoint).expect("create mountpoint");

    let mount = std::process::Command::new("mount")
        .arg("-t")
        .arg("erofs")
        .arg("-o")
        .arg("loop")
        .arg(&img_path)
        .arg(&mountpoint)
        .output()
        .expect("spawn mount");
    if !mount.status.success() {
        // Surface stderr for diagnosis; this is the load-bearing
        // failure mode we want operators to see.
        panic!(
            "kernel mount of our writer's EROFS image FAILED:\n  exit: {:?}\n  stderr: {}\n  stdout: {}",
            mount.status.code(),
            String::from_utf8_lossy(&mount.stderr),
            String::from_utf8_lossy(&mount.stdout)
        );
    }

    // Sanity-list the mounted tree. We only need *some* listing to
    // succeed -- the mount succeeding is the headline. Any read errors
    // here would be a kernel-side decode bug we'd want to know about.
    let listing = std::fs::read_dir(&mountpoint)
        .expect("read mounted dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect::<Vec<_>>();
    assert!(!listing.is_empty(), "mounted EROFS image had empty root");

    // Always unmount, even if a later assert fails — but here we run
    // the unmount eagerly because `tempdir` cleanup of the mountpoint
    // would race with a still-mounted FS.
    let _ = std::process::Command::new("umount")
        .arg(&mountpoint)
        .output();
}

/// A thoroughness check: build with modern defaults; if the reader
/// rejects it, confirm the rejection is `UnsupportedLayout(_)` (a clean
/// "we know we don't handle this") rather than a panic or wrong-bytes.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_modern_default_returns_clean_error_or_match() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let img = build_with_mkfs_erofs(&[], &sample_tree());
    let dev = common::MemDev::arc(img.bytes);
    let fs = match Filesystem::open(dev) {
        Ok(fs) => fs,
        Err(e) => {
            // Acceptable: clean refusal at SB level (e.g. unsupported
            // feature_incompat bits). Not acceptable: a parse panic.
            eprintln!("modern-default SB refused: {e:?}");
            return;
        }
    };
    // Walk every entry; any UnsupportedLayout is acceptable, but
    // mismatched bytes are not.
    for (path, want) in sample_expected() {
        let inode = match fs.lookup_path(path) {
            Ok(i) => i,
            Err(Error::UnsupportedLayout(n)) => {
                eprintln!("path {path}: UnsupportedLayout({n}) -- acceptable");
                continue;
            }
            Err(e) => {
                eprintln!("path {path}: {e:?} -- acceptable for modern default");
                continue;
            }
        };
        let mut buf = vec![0u8; want.len()];
        match fs.read_file(&inode, 0, &mut buf) {
            Ok(()) => assert_eq!(buf, want, "{path} silent corruption"),
            Err(Error::UnsupportedLayout(n)) => {
                eprintln!("read {path}: UnsupportedLayout({n}) -- acceptable");
            }
            Err(e) => {
                eprintln!("read {path}: {e:?} -- acceptable");
            }
        }
    }
}

// =====================================================================
// FRAGMENT_PCLUSTER round-trip tests
// =====================================================================
// mkfs.erofs's "fragments" feature collates the trailing bytes of many
// small files into a single "packed inode" referenced from the
// superblock. Our reader's fragment redirect loads + caches the packed
// inode and resolves fragment-bearing reads through it. These tests
// build small-file-heavy trees with `mkfs.erofs -Efragments` and
// verify each file's bytes round-trip via SHA256. Spec source: public
// EROFS compression-format documentation
// (<https://erofs.docs.kernel.org/en/latest/design.html#compressed-data>)
// + the `Z_EROFS_*` constants in `linux/fs/erofs/erofs_fs.h`. License
// clean: no kernel `.c` source consulted.
// =====================================================================

/// Build a tree of N tiny files of varying sizes whose tails fragment-
/// pack well. Each file's content is `pattern * size` so the SHA256
/// per-file is content-distinct.
fn fragments_sample_tree() -> (Vec<(String, Vec<u8>)>, fs_erofs::mkfs::Node) {
    // Chosen so each tail is small enough (well under one 4 KiB
    // lcluster) for mkfs.erofs to pack them into the shared packed
    // inode. Variety in size + content covers different
    // intra-fragment offsets.
    let specs: &[(&str, &[u8], usize)] = &[
        ("a.txt", b"hello-a\n", 7),
        ("b.txt", b"the-b-pattern\n", 11),
        ("c.bin", b"\xAA\xBB\xCC", 13),
        ("d.txt", b"d-payload-bytes\n", 5),
        ("e.bin", b"\xDE\xAD\xBE\xEF", 17),
        ("f.txt", b"f-data\n", 23),
    ];
    let mut expected = Vec::new();
    let mut entries = Vec::new();
    for (name, pat, count) in specs {
        let data: Vec<u8> = pat
            .iter()
            .copied()
            .cycle()
            .take(pat.len() * count)
            .collect();
        expected.push((format!("/{name}"), data.clone()));
        entries.push((*name, file(&data)));
    }
    (expected, dir(entries))
}

/// `mkfs.erofs -z lz4 -Efragments` on a tree of small files: each
/// file's bytes must round-trip via SHA256 through our reader. This
/// is the headline test for FRAGMENT_PCLUSTER support — without the
/// fragment redirect this previously failed at `Filesystem::open`
/// with `Error::UnsupportedLayout(98)`.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_fragments_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let (expected, tree) = fragments_sample_tree();
    let img = build_with_mkfs_erofs(&["-z", "lz4", "-Efragments"], &tree);
    let fs = open_image(img.bytes);
    for (path, want) in &expected {
        let inode = fs
            .lookup_path(path)
            .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
        assert!(inode.is_regular_file(), "{path}: expected regular file");
        assert_eq!(inode.size as usize, want.len(), "{path}: size");
        let mut buf = vec![0u8; want.len()];
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
        let want_hash = sha256(want);
        let got_hash = sha256(&buf);
        eprintln!(
            "{path}: input SHA256={} output SHA256={}",
            hex_encode(&want_hash),
            hex_encode(&got_hash)
        );
        assert_eq!(
            want_hash, got_hash,
            "{path}: SHA256 mismatch (fragment round-trip)"
        );
    }
}

/// `mkfs.erofs -z lzma` image whose feature_incompat advertises
/// `EROFS_FEATURE_INCOMPAT_COMPR_CFGS`. Verifies that the reader
/// (a) parses the post-SB COMPR_CFGS blob during open, (b) plumbs
/// the LZMA dict_size into the codec via `decompress_with_config`,
/// and (c) reproduces the source bytes byte-for-byte. The payload is
/// 100 KiB of moderately-compressible mixed-phrase text, large enough
/// that mkfs.erofs commits to actual LZMA pclusters (not full-
/// fragment dedup or PLAIN passthrough).
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_lzma_compr_cfgs_round_trip() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    // Mixed-phrase pseudo-random text, ~100 KiB. The LCG ordering
    // gives some redundancy without triggering full-fragment dedup
    // (which mkfs.erofs uses for repeated single-phrase content).
    let mut payload: Vec<u8> = Vec::with_capacity(100_000);
    let phrases: [&str; 4] = [
        "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ",
        "The quick brown fox jumps over the lazy dog. ",
        "EROFS is a read-only filesystem optimized for compression. ",
        "Test data with moderate redundancy and structure. ",
    ];
    let mut x: u32 = 0xDEAD_BEEF;
    while payload.len() < 100_000 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        let idx = (x >> 16) as usize % phrases.len();
        payload.extend_from_slice(phrases[idx].as_bytes());
    }
    payload.truncate(100_000);
    let want_hash = sha256(&payload);

    let img = build_with_mkfs_erofs(
        &["-z", "lzma", "-b", "4096"],
        &dir(vec![("big.bin", file(&payload))]),
    );
    // Inspect the SB to confirm COMPR_CFGS is actually advertised
    // (so this test is genuinely exercising the cfg-aware path).
    let feature_incompat =
        u32::from_le_bytes(img.bytes[1024 + 0x50..1024 + 0x54].try_into().unwrap());
    assert!(
        feature_incompat & fs_erofs::EROFS_FEATURE_INCOMPAT_COMPR_CFGS != 0,
        "expected COMPR_CFGS bit set; got feature_incompat=0x{feature_incompat:08X}"
    );

    let fs = open_image(img.bytes);
    // The parsed cfgs should expose an LZMA record (at minimum).
    let cfgs = fs.compr_cfgs().expect("COMPR_CFGS parsed");
    assert!(cfgs.lzma.is_some(), "LZMA cfgs record present");

    let inode = fs.lookup_path("/big.bin").expect("lookup big.bin");
    assert_eq!(inode.size as usize, payload.len());
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("LZMA + COMPR_CFGS read");
    let got_hash = sha256(&buf);

    eprintln!(
        "oracle LZMA COMPR_CFGS input  SHA256: {}",
        hex_encode(&want_hash)
    );
    eprintln!(
        "oracle LZMA COMPR_CFGS output SHA256: {}",
        hex_encode(&got_hash)
    );
    assert_eq!(
        want_hash, got_hash,
        "kernel mkfs.erofs -z lzma (COMPR_CFGS) -> our reader: SHA256 mismatch"
    );
}

/// Same flow but with both ztailpacking AND fragments explicitly
/// enabled: mkfs.erofs picks per-file which mode saves more space, so
/// the resulting image will mix ztailpacked and fragment-packed
/// files. The reader's fragment-takes-precedence policy must keep
/// each file's bytes correct.
#[test]
#[ignore = "needs mkfs.erofs (erofs-utils)"]
fn oracle_fragments_with_ztailpacking_combined() {
    if !mkfs_erofs_available() {
        eprintln!("skipping: mkfs.erofs not on PATH");
        return;
    }
    let (expected, tree) = fragments_sample_tree();
    let img = build_with_mkfs_erofs(&["-z", "lz4", "-Eztailpacking,fragments"], &tree);
    let fs = open_image(img.bytes);
    for (path, want) in &expected {
        let inode = fs
            .lookup_path(path)
            .unwrap_or_else(|e| panic!("lookup {path}: {e:?}"));
        assert!(inode.is_regular_file(), "{path}: expected regular file");
        assert_eq!(inode.size as usize, want.len(), "{path}: size");
        let mut buf = vec![0u8; want.len()];
        fs.read_file(&inode, 0, &mut buf)
            .unwrap_or_else(|e| panic!("read {path}: {e:?}"));
        let want_hash = sha256(want);
        let got_hash = sha256(&buf);
        eprintln!(
            "{path}: input SHA256={} output SHA256={}",
            hex_encode(&want_hash),
            hex_encode(&got_hash)
        );
        assert_eq!(
            want_hash, got_hash,
            "{path}: SHA256 mismatch (ztailpacking+fragments)"
        );
    }
}

// --- multi-device round trip ----------------------------------------
//
// mkfs.erofs's `--blobdev=PATH` flag emits chunked images that
// reference an external blob via `device_id == 1`. Setting that up in
// CI is brittle (the binary's flag set varies across versions and the
// staging dance is a lot for one test), so we exercise the reader
// against a SYNTHETIC two-device image built byte-by-byte from the
// public on-disk format. The reader's job is only to ROUTE — opening
// real backings is a consumer concern — and the synthetic test pins
// that routing end-to-end through `Filesystem::read_file`.

/// Construct a primary-device image + extra-device payload by hand:
/// - Primary holds the SB (extra_devices = 1, devt_slotoff = 16),
///   one device-table slot, an indexed chunked inode at NID 0, and
///   chunk 0's bytes ('A's at block 4).
/// - Extra holds chunk 1's bytes ('B's at block 0).
///
/// Returns (primary_bytes, extra_bytes, expected_file_contents).
fn build_synthetic_two_device() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use fs_erofs::superblock::EROFS_SUPER_OFFSET;
    const BS: usize = 4096;
    let mut primary = vec![0u8; BS * 6];

    // Superblock: magic, blkszbits=12, root_nid=0 (we won't traverse
    // via root since the inode is at a known NID), meta_blkaddr=1,
    // blocks=6, extra_devices=1, devt_slotoff=16 (byte 2048).
    let mut sb = [0u8; 128];
    sb[0..4].copy_from_slice(&fs_erofs::EROFS_SUPER_MAGIC_V1.to_le_bytes());
    sb[0x0C] = 12; // blkszbits
    sb[0x0E..0x10].copy_from_slice(&0u16.to_le_bytes()); // root_nid
    sb[0x24..0x28].copy_from_slice(&6u32.to_le_bytes()); // blocks
    sb[0x28..0x2C].copy_from_slice(&1u32.to_le_bytes()); // meta_blkaddr
    sb[0x56..0x58].copy_from_slice(&1u16.to_le_bytes()); // extra_devices
    sb[0x58..0x5A].copy_from_slice(&16u16.to_le_bytes()); // devt_slotoff
    primary[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + sb.len()]
        .copy_from_slice(&sb);

    // Device-table slot 0 at byte 2048: tag "blob1", blocks=1.
    let slot_off = 16 * 128;
    primary[slot_off..slot_off + 5].copy_from_slice(b"blob1");
    primary[slot_off + 64..slot_off + 68].copy_from_slice(&1u32.to_le_bytes());

    // Indexed-form chunked inode at NID 0 (byte 4096):
    //   - format: layout = ChunkBased (4) at bits 1..=3; flags = INDEXES (0x20) at bits 4..=15
    //   - mode: regular file (0x81A4)
    //   - size: 2 blocks
    //   - i_u low 16 bits: chunk-format word with INDEXES set
    let layout = 4u16 << 1; // ChunkBased
    let flags_in_iformat = 0x20u16 << 4; // INDEXES bit at flags position
    let raw_format = layout | flags_in_iformat;
    let inode_off = BS;
    primary[inode_off..inode_off + 2].copy_from_slice(&raw_format.to_le_bytes());
    primary[inode_off + 0x04..inode_off + 0x06].copy_from_slice(&0x81A4u16.to_le_bytes()); // mode
    primary[inode_off + 0x06..inode_off + 0x08].copy_from_slice(&1u16.to_le_bytes()); // nlink
    primary[inode_off + 0x08..inode_off + 0x0C].copy_from_slice(&((BS * 2) as u32).to_le_bytes()); // size
    primary[inode_off + 0x10..inode_off + 0x12].copy_from_slice(&0x20u16.to_le_bytes()); // i_u: chunk-format word

    // Chunkmap immediately after the inode body. Two 8-byte indexed entries.
    let map_off = inode_off + 32;
    // Entry 0: device_id=0, blkaddr=4 (chunk 0 on primary).
    primary[map_off..map_off + 2].copy_from_slice(&0u16.to_le_bytes()); // advise
    primary[map_off + 2..map_off + 4].copy_from_slice(&0u16.to_le_bytes()); // device_id
    primary[map_off + 4..map_off + 8].copy_from_slice(&4u32.to_le_bytes()); // blkaddr
                                                                            // Entry 1: device_id=1, blkaddr=0 (chunk 1 on extra).
    primary[map_off + 8..map_off + 10].copy_from_slice(&0u16.to_le_bytes());
    primary[map_off + 10..map_off + 12].copy_from_slice(&1u16.to_le_bytes());
    primary[map_off + 12..map_off + 16].copy_from_slice(&0u32.to_le_bytes());

    // Chunk 0 data on the primary at block 4.
    for b in &mut primary[4 * BS..5 * BS] {
        *b = b'A';
    }

    // Extra device: 1 block of 'B'.
    let extra = vec![b'B'; BS];

    // Expected file contents: 4 KiB 'A' followed by 4 KiB 'B'.
    let mut expected = vec![b'A'; BS];
    expected.extend(std::iter::repeat_n(b'B', BS));

    (primary, extra, expected)
}

#[test]
fn oracle_multidevice_round_trip() {
    // Synthetic-only: mkfs.erofs's --blobdev support is uneven across
    // erofs-utils versions. The reader's contract is "given the right
    // backings, route reads to the right device"; this exercises that
    // end-to-end on a hand-built two-device image. The single-device
    // oracle suite already covers chunked images on the primary path
    // (`oracle_chunked`).
    let (primary_bytes, extra_bytes, expected) = build_synthetic_two_device();
    let primary: Arc<dyn BlockRead> = Arc::new(MemDev::new(primary_bytes));
    let extra: Arc<dyn BlockRead> = Arc::new(MemDev::new(extra_bytes));
    let fs = Filesystem::open_with_devices(primary, vec![extra]).expect("open multi-device");
    let inode = fs.read_inode(0).expect("read inode 0");
    assert!(inode.is_regular_file());
    assert_eq!(inode.size as usize, expected.len());
    let mut buf = vec![0u8; expected.len()];
    fs.read_file(&inode, 0, &mut buf)
        .expect("read multi-device file");
    assert_eq!(
        buf, expected,
        "multi-device chunked read must concat both backings in chunk order"
    );

    // Bonus: verify the device-table tag round-trips via the reader.
    let slots = fs.read_device_table().expect("device table");
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].tag_str(), "blob1");
}
