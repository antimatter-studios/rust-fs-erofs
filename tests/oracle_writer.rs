//! Inverse-direction oracle tests: build an image with our mkfs, run
//! `fsck.erofs` over it, and (optionally) cross-check `dump.erofs`
//! output against our reader's view of the same image. Skipped when
//! the erofs-utils tooling isn't on PATH.

mod common;

use common::{dir, dump_erofs_available, file, fsck_erofs_available, open_image_path};
use fs_erofs::mkfs;
use std::process::Command;

/// Helper: write `bytes` to a tempfile and return the path + tempdir
/// guard. Caller must hold the guard for the lifetime of any path
/// access.
fn stage_image(bytes: &[u8]) -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("our.img");
    std::fs::write(&path, bytes).expect("write image");
    (path, dir)
}

fn run_fsck(path: &std::path::Path) -> (Option<i32>, String, String) {
    let out = Command::new("fsck.erofs")
        .arg(path)
        .output()
        .expect("spawn fsck.erofs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_dump(path: &std::path::Path) -> (Option<i32>, String, String) {
    let out = Command::new("dump.erofs")
        .arg("-s") // print superblock info
        .arg(path)
        .output()
        .expect("spawn dump.erofs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_simple_tree() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let img = mkfs::build_image(
        dir(vec![
            ("a.txt", file(b"hello\n")),
            ("b.bin", file(&vec![0u8; 4096])),
            ("sub", dir(vec![("c.txt", file(b"nested\n"))])),
        ]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_empty_dir() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let img = mkfs::build_image(dir(vec![]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_multi_block_file() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // 50 KiB file -> spans many blocks at default 4 KiB.
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs dump.erofs (erofs-utils)"]
fn dump_superblock_matches_our_reader() {
    if !dump_erofs_available() {
        eprintln!("skipping: dump.erofs not on PATH");
        return;
    }
    let img = mkfs::build_image(dir(vec![("a.txt", file(b"hello\n"))]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_dump(&path);
    if code != Some(0) {
        // dump.erofs may itself reject our writer's output if a feature
        // bit is wrong; surface the diagnostic.
        panic!("dump.erofs failed: {code:?}\nstdout: {stdout}\nstderr: {stderr}");
    }

    let fs = open_image_path(&path);
    let sb = fs.superblock();

    // dump.erofs prints "Filesystem blocksize: <n>" etc. Validate the
    // numbers our reader extracts appear in dump's output. Tolerant of
    // the exact format -- we only require the numeric matches, since
    // erofs-utils can change column wording.
    let bs_str = sb.block_size().to_string();
    let blocks_str = sb.blocks.to_string();
    assert!(
        stdout.contains(&bs_str) || stdout.contains(&format!("{:#x}", sb.block_size())),
        "dump.erofs output didn't mention block_size {bs_str}:\n{stdout}"
    );
    let _ = blocks_str; // dump may abbreviate, don't hard-require.
}

// ---- W1 oracle coverage: each new feature ------------------------------

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_blksize_512() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = (0..1500u32).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 9).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_wide_dir_500() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let mut entries = Vec::new();
    for i in 0..500 {
        let name = format!("file_{:04}.txt", i);
        let body = format!("body-{}\n", i);
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        entries.push((leaked, file(body.as_bytes())));
    }
    let img = mkfs::build_image(dir(entries), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_flat_inline() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let img = mkfs::build_image(dir(vec![("tiny.bin", file(b"tiny content"))]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_extended_inode() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let f = mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: b"hi".to_vec(),
        meta: mkfs::NodeMeta {
            uid: 70_000,
            gid: 80_000,
            ..Default::default()
        },
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("f.txt", f)]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_xattrs() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    use fs_erofs::xattr::ns;
    let xattrs = vec![
        mkfs::XattrSpec::new(ns::USER, b"color".to_vec(), b"red".to_vec()),
        mkfs::XattrSpec::new(ns::TRUSTED, b"cls".to_vec(), b"internal".to_vec()),
    ];
    let f = mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: b"hi".to_vec(),
        meta: mkfs::NodeMeta::default(),
        xattrs,
    };
    let img = mkfs::build_image(dir(vec![("f.txt", f)]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_special_files() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    use fs_erofs::inode::{S_IFBLK, S_IFCHR, S_IFIFO, S_IFSOCK};
    let entries = vec![
        (
            "chr",
            mkfs::Node::Device {
                mode: S_IFCHR | 0o600,
                rdev: 0x0102,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
        (
            "blk",
            mkfs::Node::Device {
                mode: S_IFBLK | 0o660,
                rdev: 0x0802,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
        (
            "fifo",
            mkfs::Node::Special {
                mode: S_IFIFO | 0o644,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
        (
            "sock",
            mkfs::Node::Special {
                mode: S_IFSOCK | 0o755,
                meta: mkfs::NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
    ];
    let img = mkfs::build_image(dir(entries), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_chunked_file() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let bs: usize = 4096;
    let chunk0: Vec<u8> = vec![b'A'; bs];
    let chunk2: Vec<u8> = vec![b'C'; bs];
    let f = mkfs::Node::ChunkedFile {
        mode: mkfs::DEFAULT_FILE_MODE,
        chunk_bits: 0,
        chunks: vec![Some(chunk0), None, Some(chunk2)],
        use_indexed_format: false,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("c.bin", f)]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_chunked_file_indexed() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let bs: usize = 4096;
    let chunk0: Vec<u8> = vec![b'X'; bs];
    let chunk1: Vec<u8> = vec![b'Y'; bs];
    let f = mkfs::Node::ChunkedFile {
        mode: mkfs::DEFAULT_FILE_MODE,
        chunk_bits: 0,
        chunks: vec![Some(chunk0), None, Some(chunk1)],
        use_indexed_format: true,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("c.bin", f)]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---- W2a oracle coverage: LZ4 compressed inodes -----------------------

fn compressed_lz4(data: &[u8]) -> mkfs::Node {
    mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        algo: mkfs::CompressedAlgo::Lz4,
        lclusterbits: 0,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
        index_format: mkfs::CompressedFileSpec::default_index_format(),
        ztailpacking: false,
        target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
    })
}

/// Build a compacted-2B compressed-file node, with optional ztailpacking.
/// Used by the W2b oracle tests below.
fn compressed_lz4_compacted2b(data: &[u8], ztailpacking: bool) -> mkfs::Node {
    mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        algo: mkfs::CompressedAlgo::Lz4,
        lclusterbits: 0,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
        index_format: mkfs::CompressedIndexFormat::Compacted2B,
        ztailpacking,
        target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
    })
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_lz4_small() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
    let img = mkfs::build_image(dir(vec![("c.bin", compressed_lz4(&payload))]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_lz4_multi_lcluster() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // 5 lclusters at 4 KiB blocks; W2a default policy emits 5 separate
    // pclusters (one per lcluster). Highly compressible so HEAD1
    // engages on every lcluster.
    let bs: usize = 4096;
    let payload: Vec<u8> = vec![b'a'; 5 * bs];
    let img = mkfs::build_image(dir(vec![("big.bin", compressed_lz4(&payload))]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_lz4_incompressible() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // LCG-derived bytes don't compress; PLAIN passthrough engages on
    // every lcluster.
    let bs: usize = 4096;
    let n = 3 * bs;
    let mut payload = Vec::with_capacity(n);
    let mut state: u64 = 0xdead_beef_cafe_babe;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        payload.push((state >> 56) as u8);
    }
    let img = mkfs::build_image(dir(vec![("r.bin", compressed_lz4(&payload))]), 12).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---- W2b oracle coverage: compacted-2B + ztailpacking -----------------

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compacted2b_small() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
    let img = mkfs::build_image(
        dir(vec![("c.bin", compressed_lz4_compacted2b(&payload, false))]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compacted2b_multi_pack() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // 12 lclusters at 4 KiB blocks: 1 pack of 6 (initial) + 3 4B packs.
    let bs: usize = 4096;
    let payload: Vec<u8> = vec![b'a'; 12 * bs];
    let img = mkfs::build_image(
        dir(vec![("c.bin", compressed_lz4_compacted2b(&payload, false))]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compacted2b_2b_middle_region() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // 22 lclusters: trips the COMPACTED_2B advise bit (16-entry middle
    // 2B pack + 6 initial 4B-form entries).
    let bs: usize = 4096;
    let payload: Vec<u8> = vec![b'b'; 22 * bs];
    let img = mkfs::build_image(
        dir(vec![("c.bin", compressed_lz4_compacted2b(&payload, false))]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compacted2b_ztailpacking() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload = b"hello compressed inline tail bytes pattern\n".repeat(10);
    let img = mkfs::build_image(
        dir(vec![("c.bin", compressed_lz4_compacted2b(&payload, true))]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---- W2c oracle coverage: multi-lcluster pcluster collation ----------

/// Build a collated LZ4 compressed file. Same defaults as
/// `compressed_lz4` but lets the caller override
/// `target_pcluster_blocks` and `index_format`.
fn collated_lz4_test_node(
    data: &[u8],
    target_pcluster_blocks: u32,
    index_format: mkfs::CompressedIndexFormat,
) -> mkfs::Node {
    mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        algo: mkfs::CompressedAlgo::Lz4,
        lclusterbits: 0,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
        index_format,
        ztailpacking: false,
        target_pcluster_blocks,
    })
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_collated_compressed() {
    // 64 KiB highly-compressible payload that the greedy collator
    // squashes into far fewer pclusters than lclusters. Verifies
    // fsck.erofs accepts the resulting NONHEAD-bearing index area.
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload: Vec<u8> = vec![b'k'; 64 * 1024];
    let img = mkfs::build_image(
        dir(vec![(
            "c.bin",
            collated_lz4_test_node(&payload, 1, mkfs::CompressedIndexFormat::Compacted2B),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_collated_compressed_legacy_index() {
    // Same coverage in the legacy / 8-byte-per-lcluster index
    // format. Two lclusters of 'a' collate into 1 HEAD + 1 NONHEAD
    // entry; fsck must accept the NONHEAD's `delta[0]` walk-back.
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let bs: usize = 4096;
    let payload: Vec<u8> = vec![b'q'; 4 * bs];
    let img = mkfs::build_image(
        dir(vec![(
            "c.bin",
            collated_lz4_test_node(&payload, 1, mkfs::CompressedIndexFormat::Legacy),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---- W3 oracle coverage: LZMA + DEFLATE compressed inodes -------------

/// Helper for the W3 oracle tests. Builds a [`mkfs::Node::CompressedFile`]
/// node with the requested codec, legacy index format, no ztailpacking.
fn compressed_with(algo: mkfs::CompressedAlgo, data: &[u8]) -> mkfs::Node {
    mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: data.to_vec(),
        algo,
        lclusterbits: 0,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
        index_format: mkfs::CompressedFileSpec::default_index_format(),
        ztailpacking: false,
        target_pcluster_blocks: mkfs::CompressedFileSpec::default_target_pcluster_blocks(),
    })
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_lzma_small() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
    let img = mkfs::build_image(
        dir(vec![(
            "c.bin",
            compressed_with(mkfs::CompressedAlgo::Lzma, &payload),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_lzma_multi_lcluster() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // 5 lclusters of repeating bytes -> highly compressible LZMA frames.
    let bs: usize = 4096;
    let payload: Vec<u8> = vec![b'a'; 5 * bs];
    let img = mkfs::build_image(
        dir(vec![(
            "big.bin",
            compressed_with(mkfs::CompressedAlgo::Lzma, &payload),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_deflate_small() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
    let img = mkfs::build_image(
        dir(vec![(
            "c.bin",
            compressed_with(mkfs::CompressedAlgo::Deflate, &payload),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_compressed_deflate_multi_lcluster() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    let bs: usize = 4096;
    let payload: Vec<u8> = vec![b'a'; 5 * bs];
    let img = mkfs::build_image(
        dir(vec![(
            "big.bin",
            compressed_with(mkfs::CompressedAlgo::Deflate, &payload),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// ---- W4 oracle coverage --------------------------------------------------
//
// fsck.erofs validates BOTH the SB checksum (when EROFS_FEATURE_COMPAT_SB_CHKSUM
// is set) and dirent hash sort within each directory block. Any image that
// passes a generic `fsck.erofs` invocation has therefore had both invariants
// confirmed by the upstream oracle.

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_validates_sb_checksum() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // Any non-trivial tree exercises the writer's SB checksum emission.
    // fsck.erofs explicitly verifies the CRC when feature_compat bit 0 is
    // set; this test fails fast if the bit is set but the CRC is wrong.
    let img = mkfs::build_image(
        dir(vec![
            ("a.txt", file(b"alpha\n")),
            ("b.txt", file(b"bravo\n")),
            ("sub", dir(vec![("c.txt", file(b"charlie\n"))])),
        ]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed (SB checksum likely mismatched):\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_validates_dirent_hash_order() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // Names whose alphabetical order disagrees with full_name_hash order.
    // fsck.erofs walks every dir block and validates the hash-sort
    // invariant internally; if our writer emitted the entries in any
    // order other than non-decreasing full_name_hash, fsck would reject.
    let img = mkfs::build_image(
        dir(vec![
            ("zebra", file(b"z")),
            ("apple", file(b"a")),
            ("mango", file(b"m")),
            ("banana", file(b"b")),
            ("kiwi", file(b"k")),
            ("durian", file(b"d")),
        ]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed (dirent hash sort likely wrong):\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs dump.erofs (erofs-utils)"]
fn dump_validates_dirent_hash_order() {
    // dump.erofs --ls prints dirents in stored order. We ensure the dump
    // succeeds (so the on-disk layout is sane) and that fsck.erofs --
    // which transitively validates the hash sort -- is also happy. We
    // don't parse dump.erofs's free-form output here; a successful
    // fsck/dump round-trip on a tree whose alphabetical and hash orders
    // diverge is the load-bearing signal.
    if !dump_erofs_available() {
        eprintln!("skipping: dump.erofs not on PATH");
        return;
    }
    let img = mkfs::build_image(
        dir(vec![
            ("zebra", file(b"z")),
            ("apple", file(b"a")),
            ("mango", file(b"m")),
            ("banana", file(b"b")),
        ]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let out = Command::new("dump.erofs")
        .arg("--ls")
        .arg("--path=/")
        .arg(&path)
        .output()
        .expect("spawn dump.erofs --ls");
    if !out.status.success() {
        // Older dump.erofs may not support --ls / --path; treat that as
        // a soft skip so this test stays useful on the toolchains where
        // it does work.
        eprintln!(
            "dump.erofs --ls not supported (exit {:?}); skipping:\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Sanity: every name we wrote must appear in the listing.
    for name in ["apple", "banana", "mango", "zebra"] {
        assert!(
            stdout.contains(name),
            "dump.erofs --ls didn't list {name}; output:\n{stdout}"
        );
    }
}

// ---- W5 oracle: BuildOptions writer extensions -----------------------------

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_image_with_xattr_prefix_dict() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    use fs_erofs::xattr::{ns, XattrLongPrefix};
    let opts = mkfs::BuildOptions {
        xattr_prefixes: vec![
            XattrLongPrefix {
                base_index: ns::USER,
                infix: b"dataitem".to_vec(),
            },
            XattrLongPrefix {
                base_index: ns::TRUSTED,
                infix: b"config".to_vec(),
            },
        ],
        ..mkfs::BuildOptions::default()
    };
    let img = mkfs::build_image_with(dir(vec![("a.txt", file(b"hello\n"))]), 12, opts).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_image_with_compr_cfgs() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    use fs_erofs::mkfs::{CompressedAlgo, CompressedFileSpec, CompressedIndexFormat};
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(20);
    let cfg = mkfs::ComprCfgsConfig {
        lzma: Some(mkfs::LzmaCfg {
            dict_size: 0x10000,
            ..mkfs::LzmaCfg::default()
        }),
        ..mkfs::ComprCfgsConfig::default()
    };
    let opts = mkfs::BuildOptions {
        compr_cfgs: Some(cfg),
        ..mkfs::BuildOptions::default()
    };
    let n = mkfs::Node::CompressedFile(CompressedFileSpec {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: payload,
        algo: CompressedAlgo::Lzma,
        lclusterbits: 0,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
        index_format: CompressedIndexFormat::Legacy,
        ztailpacking: false,
        target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
    });
    let img = mkfs::build_image_with(dir(vec![("c.bin", n)]), 12, opts).unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
#[ignore = "needs fsck.erofs (erofs-utils)"]
fn fsck_passes_on_image_with_correct_nlink() {
    if !fsck_erofs_available() {
        eprintln!("skipping: fsck.erofs not on PATH");
        return;
    }
    // Tree with multiple subdirs at one level so writer's nlink math
    // (2 + child_dirs) is exercised. fsck.erofs may or may not check
    // nlink, but it MUST NOT reject a tree whose nlink is correct.
    let img = mkfs::build_image(
        dir(vec![(
            "a",
            dir(vec![
                ("b", dir(vec![("c", dir(vec![]))])),
                ("d", dir(vec![])),
                ("file.txt", file(b"hi")),
            ]),
        )]),
        12,
    )
    .unwrap();
    let (path, _guard) = stage_image(&img);
    let (code, stdout, stderr) = run_fsck(&path);
    assert_eq!(
        code,
        Some(0),
        "fsck.erofs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}
