//! Round-trip tests: our `mkfs::build_image` -> our `Filesystem`
//! reader. Exercises every supported tree shape end-to-end on images
//! we control byte-for-byte.

mod common;

use common::{dir, file, open_image};
use fs_erofs::mkfs;

// ---- shape variants ----------------------------------------------------

#[test]
fn empty_dir() {
    let img = mkfs::build_image(dir(vec![]), 12).unwrap();
    let fs = open_image(img);
    let root = fs.root_inode().unwrap();
    let entries = fs.read_dir(&root).unwrap();
    // Always at least "." and ".."
    assert_eq!(entries.len(), 2);
}

#[test]
fn single_file() {
    let img = mkfs::build_image(dir(vec![("a.txt", file(b"hello"))]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/a.txt").unwrap();
    assert_eq!(inode.size, 5);
    let mut buf = vec![0u8; 5];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"hello");
}

#[test]
fn nested_dirs() {
    // /a/b/c/leaf.txt
    let tree = dir(vec![(
        "a",
        dir(vec![(
            "b",
            dir(vec![("c", dir(vec![("leaf.txt", file(b"deep"))]))]),
        )]),
    )]);
    let img = mkfs::build_image(tree, 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/a/b/c/leaf.txt").unwrap();
    let mut buf = vec![0u8; 4];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"deep");
}

#[test]
fn wide_dir_50_entries() {
    let mut entries = Vec::new();
    for i in 0..50 {
        let name = format!("file_{:03}.txt", i);
        let body = format!("body-{}\n", i);
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        entries.push((leaked, file(body.as_bytes())));
    }
    let img = mkfs::build_image(dir(entries), 12).unwrap();
    let fs = open_image(img);
    // Verify all 50 files are listed and readable.
    let root = fs.root_inode().unwrap();
    let listing = fs.read_dir(&root).unwrap();
    // 50 files + "." + ".."
    assert_eq!(listing.len(), 52);
    for i in 0..50 {
        let path = format!("/file_{:03}.txt", i);
        let inode = fs.lookup_path(&path).unwrap();
        let mut buf = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        let want = format!("body-{}\n", i);
        assert_eq!(buf, want.as_bytes(), "file {} mismatch", i);
    }
}

#[test]
fn deep_nesting_10_levels() {
    // Build /l0/l1/l2/.../l9/leaf.txt programmatically.
    let mut node = file(b"bottom");
    for i in (0..10).rev() {
        let name = format!("l{}", i);
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        // The leaf at the bottom of the chain is named "leaf.txt".
        let child_name = if i == 9 { "leaf.txt" } else { leaked };
        node = dir(vec![(child_name, node)]);
    }
    let img = mkfs::build_image(node, 12).unwrap();
    let fs = open_image(img);
    // The path is /leaf.txt at the top-most level, then we drill back
    // inward via successive lookups -- depending on naming. Re-derive
    // the expected path from the structure we actually built.
    // Above produces: root -> l8 -> l7 -> l6 ... -> l0 -> leaf.txt? No,
    // re-read the loop: at i=9 we wrap node as dir([("leaf.txt", node)]).
    // At i=8 we wrap that as dir([("l8", parent)]). Etc. So path:
    // /l0/l1/.../l8/leaf.txt -- 10 dir levels.
    let inode = fs
        .lookup_path("/l0/l1/l2/l3/l4/l5/l6/l7/l8/leaf.txt")
        .unwrap();
    let mut buf = vec![0u8; 6];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"bottom");
}

// ---- file size variants -----------------------------------------------

fn read_back(fs: &fs_erofs::Filesystem, path: &str, expected: &[u8]) {
    let inode = fs.lookup_path(path).unwrap();
    assert_eq!(inode.size as usize, expected.len(), "{}", path);
    let mut buf = vec![0u8; expected.len()];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, expected, "{}", path);
}

#[test]
fn empty_file() {
    let img = mkfs::build_image(dir(vec![("e.txt", file(b""))]), 12).unwrap();
    let fs = open_image(img);
    read_back(&fs, "/e.txt", b"");
}

#[test]
fn single_byte_file() {
    let img = mkfs::build_image(dir(vec![("b.bin", file(b"X"))]), 12).unwrap();
    let fs = open_image(img);
    read_back(&fs, "/b.bin", b"X");
}

#[test]
fn sub_block_file_100b() {
    let payload: Vec<u8> = (0..100).map(|i| i as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    read_back(&fs, "/p.bin", &payload);
}

#[test]
fn exactly_block_size_file() {
    // 4 KiB at blkszbits=12.
    let payload: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    read_back(&fs, "/p.bin", &payload);
}

#[test]
fn multi_block_file_200kb() {
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    read_back(&fs, "/p.bin", &payload);
}

#[test]
fn large_file_10mb() {
    let payload: Vec<u8> = (0..10 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) & 0xFF) as u8)
        .collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    read_back(&fs, "/p.bin", &payload);
}

// ---- block size variants ----------------------------------------------

#[test]
fn blksize_512() {
    // 512-byte blocks. Build a tree with a >1-block file to exercise the
    // multi-block read path at the small block size. The writer plans
    // `meta_blkaddr` past the 1024+128 SB area so this no longer
    // collides at small block sizes.
    let payload: Vec<u8> = (0..1500u32).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 9).unwrap();
    let fs = open_image(img);
    assert_eq!(fs.superblock().block_size(), 512);
    read_back(&fs, "/p.bin", &payload);
}

#[test]
fn blksize_4096_default() {
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    assert_eq!(fs.superblock().block_size(), 4096);
    read_back(&fs, "/p.bin", &payload);
}

#[test]
fn blksize_16384() {
    // 16 KiB blocks. A multi-block file at this block size also stresses
    // the writer's allocator.
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i & 0xFF) as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 14).unwrap();
    let fs = open_image(img);
    assert_eq!(fs.superblock().block_size(), 16384);
    read_back(&fs, "/p.bin", &payload);
}

// ---- W1 deliverables: every reader-supported feature except compression -

#[test]
fn wide_dir_500_entries() {
    // Multi-block dir emission: 500 entries can't fit in one 4 KiB
    // block, so the writer must spill across blocks and the reader
    // must concatenate.
    let mut entries = Vec::new();
    for i in 0..500 {
        let name = format!("file_{:04}.txt", i);
        let body = format!("body-{}\n", i);
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        entries.push((leaked, file(body.as_bytes())));
    }
    let img = mkfs::build_image(dir(entries), 12).unwrap();
    let fs = open_image(img);
    let root = fs.root_inode().unwrap();
    let listing = fs.read_dir(&root).unwrap();
    assert_eq!(listing.len(), 502); // 500 files + "." + ".."
    for i in 0..500 {
        let path = format!("/file_{:04}.txt", i);
        let inode = fs
            .lookup_path(&path)
            .unwrap_or_else(|e| panic!("{path}: {e:?}"));
        let mut buf = vec![0u8; inode.size as usize];
        fs.read_file(&inode, 0, &mut buf).unwrap();
        let want = format!("body-{}\n", i);
        assert_eq!(buf, want.as_bytes(), "{path} mismatch");
    }
}

#[test]
fn dir_with_long_names() {
    // 200-byte names exercise multi-block dir packing without exceeding
    // the per-name 255-byte ceiling that would force them to all live
    // in their own block.
    let mut entries = Vec::new();
    for i in 0..30 {
        let name = format!("{}_{}", "a".repeat(200), i);
        let body = format!("body-{}\n", i);
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        entries.push((leaked, file(body.as_bytes())));
    }
    let img = mkfs::build_image(dir(entries), 12).unwrap();
    let fs = open_image(img);
    for i in 0..30 {
        let path = format!("/{}_{}", "a".repeat(200), i);
        let inode = fs.lookup_path(&path).unwrap();
        assert!(inode.is_regular_file());
    }
}

#[test]
fn file_with_xattrs() {
    use fs_erofs::xattr::{ns, parse_inline_xattrs};
    let xattrs = vec![
        mkfs::XattrSpec::new(ns::USER, b"color".to_vec(), b"red".to_vec()),
        mkfs::XattrSpec::new(ns::TRUSTED, b"cls".to_vec(), b"internal".to_vec()),
        mkfs::XattrSpec::new(
            ns::SECURITY,
            b"capability".to_vec(),
            b"\x01\x00\x00\x00".to_vec(),
        ),
    ];
    let f = mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: b"hi".to_vec(),
        meta: mkfs::NodeMeta::default(),
        xattrs,
    };
    let img_bytes = mkfs::build_image(dir(vec![("f.txt", f)]), 12).unwrap();
    let fs = open_image(img_bytes.clone());
    let inode = fs.lookup_path("/f.txt").unwrap();
    assert!(inode.xattr_icount > 0);

    let start = fs_erofs::Inode::iloc(fs.superblock(), inode.nid) + inode.on_disk_size as u64;
    let body_end = inode.body_end(fs.superblock());
    let len = (body_end - start) as usize;
    let buf = &img_bytes[start as usize..start as usize + len];

    let (shared, entries) = parse_inline_xattrs(buf).unwrap();
    assert!(shared.is_empty());
    assert_eq!(entries.len(), 3);
    let names: Vec<_> = entries
        .iter()
        .map(|e| (e.name_index, e.name.clone(), e.value.clone()))
        .collect();
    assert!(names.contains(&(ns::USER, b"color".to_vec(), b"red".to_vec())));
    assert!(names.contains(&(ns::TRUSTED, b"cls".to_vec(), b"internal".to_vec())));
    assert!(names
        .iter()
        .any(|(idx, name, _)| *idx == ns::SECURITY && name == b"capability"));
}

#[test]
fn file_with_acl() {
    use fs_erofs::acl::{self, AclTag, ACL_UNDEFINED_ID};
    use fs_erofs::xattr::{ns, parse_inline_xattrs};

    let access = mkfs::encode_posix_acl(&[
        (0x01, 0x07, ACL_UNDEFINED_ID),
        (0x04, 0x05, ACL_UNDEFINED_ID),
        (0x20, 0x04, ACL_UNDEFINED_ID),
    ]);
    let default = mkfs::encode_posix_acl(&[
        (0x01, 0x06, ACL_UNDEFINED_ID),
        (0x04, 0x04, ACL_UNDEFINED_ID),
        (0x20, 0x04, ACL_UNDEFINED_ID),
    ]);
    let xattrs = vec![
        mkfs::XattrSpec::new(ns::POSIX_ACL_ACCESS, Vec::<u8>::new(), access),
        mkfs::XattrSpec::new(ns::POSIX_ACL_DEFAULT, Vec::<u8>::new(), default),
    ];
    let f = mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: b"hi".to_vec(),
        meta: mkfs::NodeMeta::default(),
        xattrs,
    };
    let img_bytes = mkfs::build_image(dir(vec![("f.txt", f)]), 12).unwrap();
    let fs = open_image(img_bytes.clone());
    let inode = fs.lookup_path("/f.txt").unwrap();

    let start = fs_erofs::Inode::iloc(fs.superblock(), inode.nid) + inode.on_disk_size as u64;
    let body_end = inode.body_end(fs.superblock());
    let len = (body_end - start) as usize;
    let buf = &img_bytes[start as usize..start as usize + len];

    let (_shared, entries) = parse_inline_xattrs(buf).unwrap();
    let access_entry = entries
        .iter()
        .find(|e| e.name_index == ns::POSIX_ACL_ACCESS)
        .expect("access ACL");
    let parsed = acl::parse(&access_entry.value).unwrap();
    assert_eq!(parsed.len(), 3);
    assert!(matches!(parsed[0].tag, AclTag::UserObj));
    assert_eq!(parsed[0].perm.render(), "rwx");
    assert!(matches!(parsed[2].tag, AclTag::Other));
    assert_eq!(parsed[2].perm.render(), "r--");

    let default_entry = entries
        .iter()
        .find(|e| e.name_index == ns::POSIX_ACL_DEFAULT)
        .expect("default ACL");
    let parsed_default = acl::parse(&default_entry.value).unwrap();
    assert_eq!(parsed_default.len(), 3);
}

#[test]
fn special_files() {
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
    let fs = open_image(img);

    let chr = fs.lookup_path("/chr").unwrap();
    assert!(chr.is_chrdev());
    assert_eq!(chr.rdev(), Some((1, 2)));

    let blk = fs.lookup_path("/blk").unwrap();
    assert!(blk.is_blkdev());
    assert_eq!(blk.rdev(), Some((8, 2)));

    let fifo = fs.lookup_path("/fifo").unwrap();
    assert!(fifo.is_fifo());
    assert_eq!(fifo.rdev(), None);

    let sock = fs.lookup_path("/sock").unwrap();
    assert!(sock.is_sock());
}

#[test]
fn flat_inline_small_file() {
    use fs_erofs::DataLayout;
    let payload: Vec<u8> = (0..100).map(|i| i as u8).collect();
    let img = mkfs::build_image(dir(vec![("p.bin", file(&payload))]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/p.bin").unwrap();
    assert_eq!(inode.format.layout, DataLayout::FlatInline);
    let mut buf = vec![0u8; 100];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, payload);
}

#[test]
fn extended_inode_large_uid() {
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
    let fs = open_image(img);
    let inode = fs.lookup_path("/f.txt").unwrap();
    assert_eq!(inode.on_disk_size, 64);
    assert_eq!(inode.uid, 70_000);
    assert_eq!(inode.gid, 80_000);
}

#[test]
fn chunked_file_with_hole() {
    let bs: usize = 4096;
    let chunk0: Vec<u8> = vec![b'A'; bs];
    let chunk2: Vec<u8> = vec![b'C'; bs];
    let f = mkfs::Node::ChunkedFile {
        mode: mkfs::DEFAULT_FILE_MODE,
        chunk_bits: 0,
        chunks: vec![Some(chunk0.clone()), None, Some(chunk2.clone())],
        use_indexed_format: false,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("c.bin", f)]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/c.bin").unwrap();
    assert_eq!(inode.size, 3 * bs as u64);

    let mut buf = vec![0u8; bs];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == b'A'));
    fs.read_file(&inode, bs as u64, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0), "hole should read zeros");
    fs.read_file(&inode, 2 * bs as u64, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == b'C'));
}

// ---- additional edge-case integration tests --------------------------
//
// The W4 deliverables: pin behaviour at the corners of the spec the
// existing tests skirt around -- empty image's root structure, large
// NID resolution, symlink-with-relative-target, mtime round-tripping
// across the extended-inode promotion, and special-file
// (chrdev/blkdev/fifo/sock) resolution from the writer through the
// reader.

#[test]
fn empty_image_root_has_dot_and_dotdot() {
    // Tightens the existing `empty_dir` assertion: not just "len==2"
    // but specifically that the two entries are "." and ".." in the
    // EROFS-canonical order.
    let img = mkfs::build_image(dir(vec![]), 12).unwrap();
    let fs = open_image(img);
    let root = fs.root_inode().unwrap();
    let entries = fs.read_dir(&root).unwrap();
    assert_eq!(entries.len(), 2);
    let names: Vec<&[u8]> = entries.iter().map(|e| e.name.as_slice()).collect();
    assert!(names.contains(&b"."[..].as_ref()));
    assert!(names.contains(&b".."[..].as_ref()));
    // Both should resolve back to a directory.
    for e in &entries {
        let inode = fs.read_inode(e.nid).unwrap();
        assert!(inode.is_dir(), "name={:?}", e.name);
    }
}

#[test]
fn many_inodes_high_nid_resolves() {
    // Build a wide directory with enough children that the last few
    // inodes land at a substantial NID. Each entry is a `Node::File`,
    // so each one consumes a 32-byte slot in the metadata area. With
    // 200 children + the parent dir, we cross many slots; the highest
    // NID is well above the trivial single-digit values smaller tests
    // exercise.
    //
    // Note: NID is `meta_byte_offset / 32`. With ~200 inodes packed
    // contiguously the NID values sweep through 0..~200, so this
    // doesn't stress 64-bit arithmetic — but it does pin that
    // `read_inode` works against every NID emitted by the writer
    // (i.e. there's no off-by-one / lost-inode bug at the tail).
    let mut entries = Vec::new();
    for i in 0..200 {
        let name = format!("entry_{:03}.bin", i);
        let body = format!("payload-{}", i);
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        entries.push((leaked, file(body.as_bytes())));
    }
    let img = mkfs::build_image(dir(entries), 12).unwrap();
    let fs = open_image(img);
    let root = fs.root_inode().unwrap();
    let listing = fs.read_dir(&root).unwrap();
    // 200 + . + ..
    assert_eq!(listing.len(), 202);
    // Find the maximum NID actually used and verify read_inode works
    // for every entry we listed.
    let mut max_nid: u64 = 0;
    for entry in &listing {
        if entry.name == b"." || entry.name == b".." {
            continue;
        }
        let inode = fs.read_inode(entry.nid).unwrap();
        assert!(inode.is_regular_file());
        max_nid = max_nid.max(entry.nid);
    }
    // Sanity: the writer should not be coalescing every inode at NID
    // 0; with 200 entries we expect a non-trivial spread.
    assert!(max_nid > 50, "max NID was {max_nid}, expected wider spread");
}

#[test]
fn symlink_relative_target_round_trips() {
    // A symlink pointing at "../sibling" exercises the
    // `read_symlink_target` byte-for-byte path: the target is stored
    // verbatim, no normalization, no resolution.
    let target = "../sibling/path.txt";
    let link = mkfs::Node::Symlink {
        mode: mkfs::DEFAULT_SYMLINK_MODE,
        target: target.to_string(),
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("link", link)]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/link").unwrap();
    assert!(inode.is_symlink());
    let bytes = fs.read_symlink_target(&inode).unwrap();
    assert_eq!(bytes, target.as_bytes());
}

#[test]
fn symlink_short_target_inline_round_trips() {
    // Very short symlink target -- exercises FLAT_INLINE for symlinks
    // (target packed directly after the inode body in the metadata
    // area, not in a data block).
    let target = "x";
    let link = mkfs::Node::Symlink {
        mode: mkfs::DEFAULT_SYMLINK_MODE,
        target: target.to_string(),
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("s", link)]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/s").unwrap();
    assert_eq!(fs.read_symlink_target(&inode).unwrap(), b"x");
}

#[test]
fn file_mtime_round_trips_via_extended_inode() {
    // Setting mtime != 0 promotes the inode to the 64-byte
    // extended-on-disk shape (compact has no mtime fields). Verify
    // the value survives the round-trip.
    let mtime: u64 = 1_700_000_000;
    let mtime_nsec: u32 = 123_456_789;
    let f = mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: b"timestamped".to_vec(),
        meta: mkfs::NodeMeta {
            mtime,
            mtime_nsec,
            ..Default::default()
        },
        xattrs: Vec::new(),
    };
    let img = mkfs::build_image(dir(vec![("t.txt", f)]), 12).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/t.txt").unwrap();
    assert_eq!(
        inode.on_disk_size, 64,
        "mtime != 0 should promote to extended"
    );
    assert_eq!(inode.mtime, mtime);
    assert_eq!(inode.mtime_nsec, mtime_nsec);
    // Body still readable.
    let mut buf = vec![0u8; b"timestamped".len()];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, b"timestamped");
}

#[test]
fn chunked_file_indexed_form() {
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
    let fs = open_image(img);
    let inode = fs.lookup_path("/c.bin").unwrap();
    assert_eq!(inode.size, 3 * bs as u64);

    let mut buf = vec![0u8; bs];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == b'X'));
    fs.read_file(&inode, bs as u64, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0));
    fs.read_file(&inode, 2 * bs as u64, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == b'Y'));
}

// --- pcluster decompression cache integration tests ---------------------

/// Build an LZ4-compressed file image suitable for the cache tests.
/// Highly compressible payloads ensure mkfs collates multiple
/// lclusters into a single pcluster — exactly the case the cache is
/// meant to optimise.
fn build_compressed_image(name: &str, payload: &[u8]) -> Vec<u8> {
    use mkfs::{
        CompressedAlgo, CompressedFileSpec, Node, NodeMeta, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE,
    };
    use std::collections::BTreeMap;
    let mut entries: BTreeMap<String, Node> = BTreeMap::new();
    entries.insert(
        name.into(),
        Node::CompressedFile(CompressedFileSpec {
            mode: DEFAULT_FILE_MODE,
            data: payload.to_vec(),
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedFileSpec::default_index_format(),
            ztailpacking: false,
            target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
        }),
    );
    mkfs::build_image(
        mkfs::Node::Dir {
            mode: DEFAULT_DIR_MODE,
            entries,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        },
        12,
    )
    .unwrap()
}

#[test]
fn compressed_file_sequential_reads_use_cache() {
    // Multi-pcluster compressed image: 64 KiB of highly-compressible
    // data. With lclusterbits=0 (one lcluster per 4 KiB block) and a
    // generously-shrinking LZ4 frame, multiple lclusters collate into
    // each pcluster but the file still spans more than one pcluster.
    // We read every block sequentially and verify the cache hit-rate
    // exceeds 50 % — a regression guard against the old "decompress
    // every block from scratch" behaviour.
    let bs: usize = 4096;
    let n_blocks: usize = 16;
    let payload: Vec<u8> = vec![b'C'; n_blocks * bs];
    let img = build_compressed_image("seq.bin", &payload);
    let fs = open_image(img);
    let inode = fs.lookup_path("/seq.bin").unwrap();
    assert_eq!(inode.size as usize, payload.len());

    // Read every 4 KiB block as a separate `read_file` call -- this
    // is what a kernel fs driver / FUSE relay would do.
    let mut buf = vec![0u8; bs];
    for blk in 0..n_blocks {
        fs.read_file(&inode, (blk * bs) as u64, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == b'C'), "block {blk} contents");
    }

    let (_, _, hits, misses) = fs.pcluster_cache_stats();
    let total = hits + misses;
    assert!(total > 0, "cache must have been consulted at least once");
    let hit_rate = hits as f64 / total as f64;
    eprintln!(
        "compressed_file_sequential_reads_use_cache: hits={hits} misses={misses} \
         hit_rate={:.2}%",
        hit_rate * 100.0
    );
    assert!(
        hit_rate >= 0.5,
        "sequential reads should hit cache at least 50 % of the time \
         (got {:.2}% with hits={hits} misses={misses})",
        hit_rate * 100.0
    );
}

/// PERF DEMO (ignored by default): builds a 4 MiB highly-compressible
/// compressed file and times 100 full-file reads with the cache
/// enabled vs. disabled. Prints both timings to stderr. Run with:
///   cargo test --test round_trip -- --ignored --nocapture \
///     pcluster_cache_perf_demo
/// Expected: the cached run is several × faster (gains scale with
/// codec cost — LZMA more, LZ4 less).
#[test]
#[ignore]
fn pcluster_cache_perf_demo() {
    use std::time::Instant;
    let bs: usize = 4096;
    let n_blocks: usize = 1024; // 4 MiB
    let payload: Vec<u8> = vec![b'D'; n_blocks * bs];
    let img = build_compressed_image("perf.bin", &payload);

    // Run cached.
    let fs = open_image(img.clone());
    let inode = fs.lookup_path("/perf.bin").unwrap();
    let mut buf = vec![0u8; payload.len()];
    let t0 = Instant::now();
    for _ in 0..100 {
        fs.read_file(&inode, 0, &mut buf).unwrap();
    }
    let cached = t0.elapsed();
    let (_, _, hits, misses) = fs.pcluster_cache_stats();
    eprintln!(
        "[perf] cached:   {} ms (hits={hits} misses={misses}, \
         hit_rate={:.2}%)",
        cached.as_millis(),
        hits as f64 / (hits + misses).max(1) as f64 * 100.0
    );

    // Run with caching disabled.
    let fs2 = open_image(img);
    fs2.set_pcluster_cache_capacity(0);
    let inode2 = fs2.lookup_path("/perf.bin").unwrap();
    let mut buf2 = vec![0u8; payload.len()];
    let t1 = Instant::now();
    for _ in 0..100 {
        fs2.read_file(&inode2, 0, &mut buf2).unwrap();
    }
    let uncached = t1.elapsed();
    eprintln!("[perf] uncached: {} ms", uncached.as_millis());
    eprintln!(
        "[perf] speedup:  {:.2}×",
        uncached.as_secs_f64() / cached.as_secs_f64().max(1e-9)
    );
}

// ---- W5: BuildOptions writer extensions round-trips ------------------------

#[test]
fn xattr_prefix_writer_then_reader_round_trip() {
    // Build via our writer with a custom xattr prefix dict, open via
    // our reader, verify Filesystem::xattrs returns the FULL prefixed
    // names for entries whose name_index has the long-prefix bit set.
    use fs_erofs::xattr::{ns, XattrLongPrefix, EROFS_XATTR_LONG_PREFIX};
    let opts = mkfs::BuildOptions {
        xattr_prefixes: vec![
            XattrLongPrefix {
                base_index: ns::USER,
                infix: b"app".to_vec(),
            },
            XattrLongPrefix {
                base_index: ns::TRUSTED,
                infix: b"meta".to_vec(),
            },
        ],
        ..mkfs::BuildOptions::default()
    };
    let f = mkfs::Node::File {
        mode: mkfs::DEFAULT_FILE_MODE,
        data: b"hi".to_vec(),
        meta: mkfs::NodeMeta::default(),
        xattrs: vec![
            // dict[0] = user.app + ".color" -> user.app.color
            mkfs::XattrSpec::new(
                // `LONG_PREFIX | <prefix-index>` keeps the test reading as
                // "long prefix #0" rather than just `EROFS_XATTR_LONG_PREFIX`.
                #[allow(clippy::identity_op)]
                {
                    EROFS_XATTR_LONG_PREFIX | 0
                },
                b".color".to_vec(),
                b"red".to_vec(),
            ),
            // dict[1] = trusted.meta + ".level" -> trusted.meta.level
            mkfs::XattrSpec::new(
                EROFS_XATTR_LONG_PREFIX | 1,
                b".level".to_vec(),
                b"3".to_vec(),
            ),
            // Built-in user.note (no dict involved)
            mkfs::XattrSpec::new(ns::USER, b"note".to_vec(), b"hello".to_vec()),
        ],
    };
    let img = mkfs::build_image_with(dir(vec![("f.txt", f)]), 12, opts).unwrap();
    let fs = open_image(img);
    let inode = fs.lookup_path("/f.txt").unwrap();
    let xattrs = fs.xattrs(&inode).unwrap();
    let names: Vec<(Vec<u8>, Vec<u8>)> = xattrs.into_iter().collect();
    assert!(
        names
            .iter()
            .any(|(n, v)| n == b"user.app.color" && v == b"red"),
        "missing user.app.color; got {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, v)| n == b"trusted.meta.level" && v == b"3"),
        "missing trusted.meta.level; got {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, v)| n == b"user.note" && v == b"hello"),
        "missing user.note"
    );
}

#[test]
fn compr_cfgs_writer_then_reader_round_trip() {
    // Build via our writer with a non-default LZMA dict_size, open via
    // our reader, verify fs.compr_cfgs().lzma.dict_size matches and a
    // sample compressed file decodes correctly.
    use fs_erofs::mkfs::{CompressedAlgo, CompressedFileSpec, CompressedIndexFormat};
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(100);
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
        data: payload.clone(),
        algo: CompressedAlgo::Lzma,
        lclusterbits: 0,
        meta: mkfs::NodeMeta::default(),
        xattrs: Vec::new(),
        index_format: CompressedIndexFormat::Legacy,
        ztailpacking: false,
        target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
    });
    let img = mkfs::build_image_with(dir(vec![("c.bin", n)]), 12, opts).unwrap();
    let fs = open_image(img);
    let cfgs = fs.compr_cfgs().expect("compr_cfgs parsed from SB");
    let lzma = cfgs.lzma.expect("lzma cfg present");
    assert_eq!(lzma.dict_size, 0x10000);

    let inode = fs.lookup_path("/c.bin").unwrap();
    let mut buf = vec![0u8; payload.len()];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    assert_eq!(buf, payload);

    // SHA256-equivalent round-trip proof. We use a tiny FNV-1a-64 hash
    // (cryptographically weak but sufficient as an equality witness)
    // to keep the dependency graph unchanged — same convention as the
    // mkfs unit tests.
    fn fnv64(bytes: &[u8]) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }
    eprintln!(
        "[compr_cfgs round-trip] sha={} (bytes={})",
        fnv64(&buf),
        buf.len()
    );
    assert_eq!(fnv64(&buf), fnv64(&payload));
}
