//! Pclusters `mkfs.erofs` wrote, taken into an image this crate builds:
//! `fsck.erofs` decompresses every one of them there, and the kernel reads
//! every file as the file that went into `mkfs.erofs`.
//!
//! `transplant.rs` holds the same of images that are this crate's own on
//! both sides, which proves only that its writer and its reader agree.
//! Here the pclusters come from erofs-utils, in each shape it writes that
//! a transplant takes -- one-block and big pclusters, the compacted and
//! the legacy index, stored blocks between compressed ones -- and what is
//! built of them is read by erofs-utils and by Linux.

mod common;

use common::{build_with_mkfs_erofs, dir, file, open_image, stage_image};
use fs_erofs::mkfs::{self, Content, Node, NodeMeta, Transplant, DEFAULT_FILE_MODE};
use fs_erofs::Filesystem;
use fs_erofs_test_support::{guest_kernel_read, oracle, sha256_hex};

/// Text that compresses about as well as a configuration file does.
fn text(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "system", "vendor", "service", "class", "user", "group", "oneshot", "disabled", "socket",
        "property", "trigger", "start", "write", "chmod", "0755", "/data",
    ];
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(WORDS[(state % 16) as usize].as_bytes());
        out.push(if state.is_multiple_of(7) { b'\n' } else { b' ' });
    }
    out.truncate(len);
    out
}

/// Bytes that do not compress.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Files `mkfs.erofs` compresses: text alone, ending inside a block and on
/// one; text with stretches that do not compress, which it stores a block
/// at a time from wherever the text before them ended; more than the
/// 4 MiB the writer holds at once; zeros, a pcluster of which holds far
/// more than it fills. And two it stores uncompressed, which a transplant
/// does not describe.
fn files() -> Vec<(&'static str, Vec<u8>)> {
    let mut mixed = text(70_001, 1);
    mixed.extend(noise(50_000, 2));
    mixed.extend(text(90_000, 3));
    mixed.extend(noise(4_097, 4));
    let mut tail = text(200_000, 11);
    tail.extend(noise(150_003, 12));
    vec![
        ("text", text(300_001, 5)),
        ("blocks", text(40 * 4096, 6)),
        ("mixed", mixed),
        ("tail", tail),
        ("segments", text((5 << 20) + 123, 7)),
        ("zeros", vec![0u8; 3 << 20]),
        ("noise", noise(100_000, 9)),
        ("inline", text(700, 10)),
    ]
}

/// The names of [`files`] that `mkfs.erofs` compresses.
const COMPRESSED: [&str; 6] = ["text", "blocks", "mixed", "tail", "segments", "zeros"];

fn source(args: &[&str]) -> Filesystem {
    let tree = dir(files()
        .iter()
        .map(|(name, data)| (*name, file(data)))
        .collect());
    open_image(build_with_mkfs_erofs(args, &tree).bytes)
}

fn read_back(fs: &Filesystem, path: &str) -> Vec<u8> {
    let inode = fs.lookup_path(path).unwrap();
    let mut buf = vec![0u8; inode.size as usize];
    fs.read_file(&inode, 0, &mut buf).unwrap();
    buf
}

/// What a rebuild took from its source.
#[derive(Default)]
struct Taken {
    /// The files transplanted.
    names: Vec<&'static str>,
    /// The blocks of the largest pcluster among them.
    largest: u32,
    /// Stored pclusters that start inside a block of their file.
    stored_inside_a_block: usize,
}

/// An image of every file of `from`: its pclusters where a transplant
/// describes them, its bytes where none does.
fn rebuilt(from: &Filesystem) -> (Vec<u8>, Taken) {
    let mut taken = Taken::default();
    let mut entries = std::collections::BTreeMap::new();
    for (name, _) in files() {
        let path = format!("/{name}");
        let inode = from.lookup_path(&path).unwrap();
        let node = match Transplant::of(from, &inode).unwrap() {
            Some(data) => {
                taken.names.push(name);
                for p in data.pclusters() {
                    taken.largest = taken.largest.max(p.blocks);
                    if !p.compressed && p.offset % 4096 != 0 {
                        taken.stored_inside_a_block += 1;
                    }
                }
                Node::TransplantedFile {
                    mode: DEFAULT_FILE_MODE,
                    data,
                    meta: NodeMeta::default(),
                    xattrs: Vec::new(),
                }
            }
            None => Node::StreamedFile {
                mode: DEFAULT_FILE_MODE,
                content: Content::from_bytes(read_back(from, &path)),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        };
        entries.insert(name.to_string(), node);
    }
    let tree = Node::Dir {
        mode: mkfs::DEFAULT_DIR_MODE,
        entries,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    };
    (mkfs::build_image(tree, 12).unwrap(), taken)
}

/// `fsck.erofs --extract` decompresses every pcluster of `image`, and
/// accepts it.
#[track_caller]
fn assert_fsck_decodes_every_file(image: &str, tag: &str) {
    let out = oracle("fsck.erofs").arg("--extract").arg(image).output();
    assert_eq!(
        out.status.code(),
        Some(0),
        "[{tag}] fsck.erofs --extract {image}:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Each shape of LZ4 image `mkfs.erofs` writes that a transplant takes:
/// its arguments, and whether its pclusters are of more than one block.
const SHAPES: [(&str, &[&str], bool); 5] = [
    ("one-block", &["-b4096", "-zlz4hc"], false),
    ("16k", &["-b4096", "-zlz4hc,12", "-C16384"], true),
    ("64k", &["-b4096", "-zlz4hc", "-C65536"], true),
    ("1m", &["-b4096", "-zlz4hc", "-C1048576"], true),
    (
        "legacy-index",
        &["-b4096", "-zlz4", "-Elegacy-compress"],
        false,
    ),
];

/// An image built of the pclusters of one `mkfs.erofs` wrote, in `shape`,
/// having held that every compressed file was taken and none other, and
/// that the pclusters were of the shape asked for.
fn transplanted_from(shape: &str) -> Vec<u8> {
    let (tag, args, big) = SHAPES
        .iter()
        .find(|(tag, ..)| *tag == shape)
        .expect("a shape of SHAPES");
    let (img, mut taken) = rebuilt(&source(args));
    taken.names.sort();
    let mut compressed = COMPRESSED.to_vec();
    compressed.sort();
    assert_eq!(taken.names, compressed, "[{tag}]");
    assert_eq!(taken.largest > 1, *big, "[{tag}] largest {}", taken.largest);
    // All but the 1 MiB pclusters, which take the noise in with the text,
    // store it a block at a time from where the text ended.
    assert_eq!(
        taken.stored_inside_a_block > 0,
        *tag != "1m",
        "[{tag}] {} stored inside a block",
        taken.stored_inside_a_block
    );
    img
}

#[test]
fn fsck_decodes_every_file_transplanted_from_what_mkfs_erofs_wrote() {
    for (tag, ..) in SHAPES {
        let img = transplanted_from(tag);
        let (path, _guard) = stage_image("transplant", &img);
        assert_fsck_decodes_every_file(path.to_str().expect("utf-8 path"), tag);
        let fs = open_image(img);
        for (name, data) in files() {
            assert!(
                read_back(&fs, &format!("/{name}")) == data,
                "[{tag}] {name} reads back different"
            );
        }
    }
}

#[test]
fn the_kernel_reads_every_file_transplanted_from_what_mkfs_erofs_wrote() {
    for (tag, ..) in SHAPES {
        let img = transplanted_from(tag);
        let (path, _guard) = stage_image("transplant", &img);
        let out = guest_kernel_read(
            path.to_str().expect("utf-8 path"),
            r#"cd "$MNT" && sha256sum -- *"#,
        );
        assert!(
            out.status.success(),
            "[{tag}] the kernel could not read the image:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let kernel = String::from_utf8(out.stdout).unwrap();
        for (name, data) in files() {
            let want = format!("{}  {name}", sha256_hex(&data));
            assert!(
                kernel.lines().any(|l| l == want),
                "[{tag}] the kernel read {name} as something else:\n{kernel}"
            );
        }
    }
}

/// What a transplant does not describe, it says so of: another codec, a
/// tail in the packed inode, a tail inline after the inode. Nothing of
/// such a file is taken; a file beside it that is stored as a transplant
/// describes still is -- with ztailpacking, one whose tail did not fit
/// after its inode -- and `fsck.erofs` decodes the image built of both.
#[test]
fn a_file_stored_as_no_transplant_describes_is_none() {
    let declined: [(&str, &[&str], &[&str]); 3] = [
        ("lzma", &["-b4096", "-zlzma"], &COMPRESSED),
        (
            "fragments",
            &["-b4096", "-zlz4hc", "-Efragments"],
            &COMPRESSED,
        ),
        (
            "ztailpacking",
            &["-b4096", "-zlz4hc", "-Eztailpacking"],
            &["text", "mixed"],
        ),
    ];
    for (tag, args, none) in declined {
        let (img, taken) = rebuilt(&source(args));
        for name in none {
            assert!(!taken.names.contains(name), "[{tag}] took {name}");
        }
        let (path, _guard) = stage_image("transplant-none", &img);
        assert_fsck_decodes_every_file(path.to_str().expect("utf-8 path"), tag);
        let fs = open_image(img);
        for (name, data) in files() {
            assert!(
                read_back(&fs, &format!("/{name}")) == data,
                "[{tag}] {name} reads back different"
            );
        }
    }
}
