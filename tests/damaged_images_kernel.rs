//! Damaged images, read by the Linux kernel and by this crate (#311).
//!
//! The salvage work this crate is taking on (docs/three-layer-api.md)
//! needs ground truth: for an image with one structure destroyed, which
//! files does an independent reader still read, and with which bytes?
//! This suite builds healthy images with `mkfs.erofs` in the harness
//! guest, destroys one structure in each copy, and asks the kernel. It
//! then asks this crate's ordinary reader the same question, path by
//! path, and the two must agree: the same bytes where the kernel reads a
//! file, an error where the kernel cannot, and a refused mount exactly
//! where this crate refuses to open the image.
//!
//! Each damaged copy must also lose something and keep something: a copy
//! the kernel reads in full was not damaged where it matters, and one it
//! reads nothing of says nothing about what survives. `fsck.erofs
//! --extract` must call every damaged copy broken, so the damage is real
//! by a second reader too.
//!
//! The damage is placed by reading the healthy image with this crate:
//! where a directory's block or a file's pcluster is. That is locating,
//! not judging -- every verdict comes from the kernel.

mod common;

use common::{build_with_mkfs_erofs, dir, file, mixed_run_files, stage_image, MemDev};
use fs_erofs::layout::DataLayout;
use fs_erofs::{Filesystem, Inode, ZMap};
use fs_erofs_test_support::{assert_fsck_clean, guest_kernel_read_if_mounted, oracle, sha256_hex};
use std::collections::BTreeMap;

const BLOCK: u64 = 4096;

/// A directory big enough to fill more than one block, so one of its
/// blocks can be destroyed while the rest of the tree is untouched.
const WIDE_ENTRIES: usize = 400;

/// The tree every image is built from: path (without the leading `/`) to
/// contents.
fn files() -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    out.insert("top.txt".to_string(), b"top of the tree\n".to_vec());
    for i in 0..WIDE_ENTRIES {
        out.insert(
            format!("wide/entry-{i:03}"),
            format!("wide entry {i}\n").into_bytes(),
        );
    }
    for (name, data) in mixed_run_files() {
        out.insert(format!("data/{name}"), data);
    }
    out
}

fn tree() -> fs_erofs::mkfs::Node<'static> {
    let files = files();
    let subdir = |prefix: &str| {
        dir(files
            .iter()
            .filter_map(|(path, data)| path.strip_prefix(prefix).map(|name| (name, file(data))))
            .collect())
    };
    dir(vec![
        ("top.txt", file(&files["top.txt"])),
        ("wide", subdir("wide/")),
        ("data", subdir("data/")),
    ])
}

/// What a reader made of one path: its SHA-256, or that it could not
/// read it.
type Reading = BTreeMap<String, Option<String>>;

/// The kernel's reading of every path, or what `mount` said when it
/// refused the image.
fn kernel_reading(image: &std::path::Path) -> Result<Reading, String> {
    let mut script = String::from(
        r#"while IFS= read -r p; do
    if h="$(sha256sum < "$MNT/$p" 2>/dev/null)"; then
        printf '%s|%s\n' "$p" "${h%% *}"
    else
        printf '%s|ERR\n' "$p"
    fi
done <<'PATHS'
"#,
    );
    for path in files().keys() {
        script.push_str(path);
        script.push('\n');
    }
    script.push_str("PATHS\n");
    let image = image.to_str().expect("utf-8 image path");
    let out = guest_kernel_read_if_mounted(image, &script)?;
    assert!(
        out.status.success(),
        "the kernel's walk of {image} failed ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut reading = Reading::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let (path, hash) = line.rsplit_once('|').expect("path|hash");
        reading.insert(path.to_string(), (hash != "ERR").then(|| hash.to_string()));
    }
    assert_eq!(
        reading.len(),
        files().len(),
        "the kernel's walk of {image} reported {} paths of {}",
        reading.len(),
        files().len()
    );
    Ok(reading)
}

/// This crate's reading of every path, or why it refused to open the
/// image.
fn driver_reading(bytes: &[u8]) -> Result<Reading, String> {
    let fs = Filesystem::open(MemDev::arc(bytes.to_vec())).map_err(|e| e.to_string())?;
    let mut reading = Reading::new();
    for path in files().keys() {
        let hash = fs.lookup_path(&format!("/{path}")).ok().and_then(|inode| {
            let mut buf = vec![0u8; inode.size as usize];
            fs.read_file(&inode, 0, &mut buf)
                .ok()
                .map(|()| sha256_hex(&buf))
        });
        reading.insert(path.clone(), hash);
    }
    Ok(reading)
}

/// Every path's SHA-256 as built.
fn expected() -> Reading {
    files()
        .into_iter()
        .map(|(path, data)| (path, Some(sha256_hex(&data))))
        .collect()
}

/// One damaged copy of `healthy`, judged by the kernel, `fsck.erofs` and
/// this crate. Returns every way the judgement went wrong, so one run
/// reports each damaged copy rather than stopping at the first.
fn judge(what: &str, healthy: &[u8], damage: impl FnOnce(&mut Vec<u8>)) -> Vec<String> {
    let mut bytes = healthy.to_vec();
    damage(&mut bytes);
    assert_ne!(bytes, healthy, "[{what}] the damage changed nothing");
    let (image, _guard) = stage_image("damaged", &bytes);
    let mut problems = Vec::new();

    // `--extract` with no directory decompresses and checks every file's
    // data and writes nothing; without it fsck.erofs checks metadata only,
    // and calls an image with a destroyed pcluster clean.
    let fsck = oracle("fsck.erofs").arg("--extract").arg(&image).output();
    if fsck.status.code() == Some(0) {
        problems.push(format!(
            "[{what}] fsck.erofs --extract calls the damaged image clean"
        ));
    }

    let kernel = kernel_reading(&image);
    let driver = driver_reading(&bytes);
    match (&kernel, &driver) {
        (Err(_), Err(_)) => {}
        (Err(said), Ok(_)) => problems.push(format!(
            "[{what}] the kernel refused the image ({}) and this crate opened it",
            said.trim()
        )),
        (Ok(_), Err(why)) => problems.push(format!(
            "[{what}] the kernel mounted the image and this crate refused it: {why}"
        )),
        (Ok(kernel), Ok(driver)) => {
            // Lost: unreadable, or read as bytes the file was not built
            // from. EROFS keeps no checksum of file data, so a destroyed
            // block stored uncompressed reads back as whatever is there,
            // by the kernel and by this crate alike.
            let expected = expected();
            let lost = kernel
                .iter()
                .filter(|(path, hash)| hash.as_ref() != expected[*path].as_ref())
                .count();
            if lost == 0 {
                problems.push(format!("[{what}] the kernel still reads every file intact"));
            }
            if lost == kernel.len() {
                problems.push(format!("[{what}] the kernel reads no file at all"));
            }
            let show = |h: Option<&Option<String>>| match h {
                Some(Some(h)) => h[..12].to_string(),
                _ => "ERR".to_string(),
            };
            let disagree: Vec<String> = kernel
                .iter()
                .filter(|(path, hash)| driver.get(*path) != Some(*hash))
                .map(|(path, hash)| {
                    format!(
                        "{path}: kernel {} crate {}",
                        show(Some(hash)),
                        show(driver.get(path))
                    )
                })
                .collect();
            if !disagree.is_empty() {
                problems.push(format!(
                    "[{what}] this crate and the kernel read {} of {} paths differently \
                     ({lost} lost by the kernel), first {:?}",
                    disagree.len(),
                    kernel.len(),
                    &disagree[..disagree.len().min(4)]
                ));
            }
        }
    }
    problems
}

/// A healthy image, checked clean by `fsck.erofs` and read in full by
/// the kernel and this crate, which agree on every file's bytes.
fn healthy(args: &[&str]) -> Vec<u8> {
    let img = build_with_mkfs_erofs(args, &tree());
    let path = img.path.to_str().expect("utf-8 image path");
    assert_fsck_clean(path, &format!("healthy {args:?}"));
    let expected = expected();
    let kernel = kernel_reading(&img.path).expect("the kernel mounts the healthy image");
    assert_eq!(
        kernel, expected,
        "{args:?}: the kernel's reading of the healthy image"
    );
    let driver = driver_reading(&img.bytes).expect("this crate opens the healthy image");
    assert_eq!(
        driver, expected,
        "{args:?}: this crate's reading of the healthy image"
    );
    img.bytes
}

fn inode_at(fs: &Filesystem, path: &str) -> Inode {
    fs.lookup_path(path)
        .unwrap_or_else(|e| panic!("{path}: {e:?}"))
}

/// Overwrite `len` bytes at `offset` with a pattern no structure decodes:
/// an impossible inode format, a `nameoff` past every block, an LZ4
/// token whose literal run never ends.
fn scribble(bytes: &mut [u8], offset: u64, len: u64) {
    let (start, end) = (offset as usize, (offset + len) as usize);
    bytes[start..end].fill(0xFF);
}

/// The block holding the first full block of `/wide`'s entries.
fn wide_dir_block(fs: &Filesystem) -> u64 {
    let wide = inode_at(fs, "/wide");
    assert!(
        wide.size > BLOCK,
        "fixture: /wide is {} bytes, not more than one block",
        wide.size
    );
    assert!(
        matches!(
            wide.format.layout,
            DataLayout::FlatPlain | DataLayout::FlatInline
        ),
        "fixture: /wide is laid out {:?}, not as plain blocks",
        wide.format.layout
    );
    u64::from(wide.raw_u) * BLOCK
}

/// The metadata block holding `/data/f20.bin`'s inode, checked to hold
/// neither the root's nor `/data`'s, so the damage removes files and not
/// the way to them.
fn file_inode_block(fs: &Filesystem) -> u64 {
    let sb = fs.superblock();
    let block_of = |inode: &Inode| Inode::iloc(sb, inode.nid) / BLOCK;
    let target = block_of(&inode_at(fs, "/data/f20.bin"));
    for keep in ["/", "/data", "/wide"] {
        assert_ne!(
            block_of(&inode_at(fs, keep)),
            target,
            "fixture: {keep}'s inode shares a block with /data/f20.bin's"
        );
    }
    target * BLOCK
}

/// The first pcluster of `/data/f04.bin`, in an image built compressed.
fn first_pcluster(fs: &Filesystem, bytes: &[u8]) -> (u64, u64) {
    let inode = inode_at(fs, "/data/f04.bin");
    assert!(
        matches!(
            inode.format.layout,
            DataLayout::Compression | DataLayout::CompressionLegacy
        ),
        "fixture: /data/f04.bin is laid out {:?}, not compressed",
        inode.format.layout
    );
    let dev = MemDev::new(bytes.to_vec());
    let zmap = ZMap::open(&dev, fs.superblock(), &inode).expect("zmap");
    let mapping = zmap.map(&dev, 0).expect("map offset 0");
    assert!(
        matches!(
            mapping.cluster_type,
            fs_erofs::zmap::Z_EROFS_LCLUSTER_TYPE_HEAD1
                | fs_erofs::zmap::Z_EROFS_LCLUSTER_TYPE_HEAD2
        ),
        "fixture: /data/f04.bin's first pcluster is stored uncompressed (type {})",
        mapping.cluster_type
    );
    (
        u64::from(mapping.pcluster_blkaddr) * BLOCK,
        u64::from(mapping.pcluster_blocks.max(1)) * BLOCK,
    )
}

/// An uncompressed image: the superblock, one directory block, one block
/// of inodes, and the image's second half each destroyed in turn.
#[test]
fn the_kernel_and_this_crate_read_the_same_files_from_a_damaged_plain_image() {
    let healthy = healthy(&["-b4096"]);
    let fs = Filesystem::open(MemDev::arc(healthy.clone())).expect("open");
    let dir_block = wide_dir_block(&fs);
    let inode_block = file_inode_block(&fs);

    let problems = [
        judge("superblock overwritten", &healthy, |b| {
            scribble(b, 1024, 128)
        }),
        judge("one block of /wide's entries overwritten", &healthy, |b| {
            scribble(b, dir_block, BLOCK)
        }),
        judge(
            "the block holding /data/f20.bin's inode overwritten",
            &healthy,
            |b| scribble(b, inode_block, BLOCK),
        ),
        judge("the image's second half cut off", &healthy, |b| {
            b.truncate(b.len() / 2)
        }),
    ]
    .concat();
    assert!(problems.is_empty(), "{problems:#?}");
}

/// An LZ4-compressed image: one pcluster destroyed, and the image's
/// second half cut off.
#[test]
fn the_kernel_and_this_crate_read_the_same_files_from_a_damaged_lz4_image() {
    let healthy = healthy(&["-b4096", "-zlz4"]);
    let fs = Filesystem::open(MemDev::arc(healthy.clone())).expect("open");
    let (pcluster, len) = first_pcluster(&fs, &healthy);

    let problems = [
        judge(
            "/data/f04.bin's first pcluster overwritten",
            &healthy,
            |b| scribble(b, pcluster, len),
        ),
        judge("the image's second half cut off", &healthy, |b| {
            b.truncate(b.len() / 2)
        }),
    ]
    .concat();
    assert!(problems.is_empty(), "{problems:#?}");
}
