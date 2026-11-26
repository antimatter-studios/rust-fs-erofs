//! `mkfs.erofs`: build an EROFS image from a directory.
//!
//! The builder that shipped as the `mkfs_erofs` target, moved into the
//! multi-call binary and ported to clap. The argument order is erofs-utils'
//! (`mkfs.erofs OUTPUT SOURCE`), and so are the short flags it shares with
//! it (`-b` for the block size in bytes, `-L` for the label).
//!
//! What is new is the result: a JSON report on stdout, read back from the
//! superblock of the image that was written, which names every source
//! entry the builder left out and why. Progress and warnings stay on
//! stderr (`--quiet` silences them); `--text` prints nothing on stdout on
//! success, as the tool did before.
//!
//! Uncompressed unless `-z` asks for LZ4 or LZ4HC, spelled as erofs-utils
//! spells them. Symlinks, FIFOs, sockets and device nodes are written as
//! what they are, and the names of one file as one inode with that many
//! links. What an image cannot hold is left out with a warning: a name or a
//! symlink target that is not UTF-8, a device number Linux has no room for,
//! and, on Windows, a special file. Windows gives no file identity here, so
//! each name of a hard-linked file is copied there.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_erofs::mkfs::{
    build_image_with, BuildOptions, CompressedAlgo, CompressedFileSpec, Compression, Node,
    NodeMeta, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE,
};
use fs_erofs::superblock::{
    is_valid_blkszbits, EROFS_SUPER_BLOCK_SIZE, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE,
};
use fs_erofs::{Superblock, EROFS_SUPER_OFFSET};

pub const TOOL: Tool = Tool {
    name: "mkfs.erofs",
    verb: "mkfs",
    section: 8,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "Build an EROFS image from a directory",
    command,
    run,
};

/// The block size when none is given.
const DEFAULT_BLOCK_SIZE: u64 = 4096;

/// The LZ4HC level `-z lz4hc` means when it names none, as erofs-utils'.
const DEFAULT_LZ4HC_LEVEL: u8 = 9;

/// What `-z` asked regular files to be compressed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Lz4,
    Lz4Hc(u8),
}

impl Codec {
    /// As the report names it.
    fn name(self) -> String {
        match self {
            Codec::Lz4 => "lz4".to_string(),
            Codec::Lz4Hc(level) => format!("lz4hc,{level}"),
        }
    }
}

/// `lz4`, `lz4hc` or `lz4hc,LEVEL`.
fn parse_codec(text: &str) -> Result<Codec, String> {
    match text.split_once(',') {
        None if text == "lz4" => Ok(Codec::Lz4),
        None if text == "lz4hc" => Ok(Codec::Lz4Hc(DEFAULT_LZ4HC_LEVEL)),
        Some(("lz4hc", level)) => level
            .parse::<u8>()
            .ok()
            .filter(|level| (1..=12).contains(level))
            .map(Codec::Lz4Hc)
            .ok_or_else(|| format!("{level:?} is not an LZ4HC level in 1..=12")),
        _ => Err(format!("{text:?} is not lz4, lz4hc or lz4hc,LEVEL")),
    }
}

fn command() -> Cmd {
    Cmd::new("mkfs.erofs")
        .about("Build an EROFS image from a directory")
        .long_about(
            "Build an EROFS image of SOURCE, a directory (or a single regular file, which \
             becomes the image's only entry), and write it to OUTPUT. Files are stored as \
             they are unless -z names a compressor.\n\n\
             Files, directories, symlinks, FIFOs, sockets and device nodes are copied \
             with their permission bits, and the names of a hardlinked file as one inode. \
             A name or symlink target that is not UTF-8, a device number the image cannot \
             hold and, on Windows, a special file are left out, each named in the report \
             and warned about on stderr. On Windows each name of a hardlinked file is \
             copied.\n\n\
             A JSON report of what was written, read back from the new superblock, goes \
             to stdout; progress and warnings go to stderr.",
        )
        .arg(
            Arg::new("output")
                .value_name("OUTPUT")
                .help("The image file to write (replaced if it exists)")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("source")
                .value_name("SOURCE")
                .help("The directory to copy into the image")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("block-size")
                .short('b')
                .long("block-size")
                .value_name("BYTES")
                .help(format!(
                    "Block size: a power of 2 in {MIN_BLOCK_SIZE}..={MAX_BLOCK_SIZE}. \
                     Default: {DEFAULT_BLOCK_SIZE}."
                ))
                .value_parser(parse_block_size),
        )
        .arg(
            Arg::new("compress")
                .short('z')
                .long("compress")
                .value_name("ALGORITHM")
                .help(format!(
                    "Compress regular files: lz4, lz4hc or lz4hc,LEVEL (1..=12, \
                     {DEFAULT_LZ4HC_LEVEL} when none is named)"
                ))
                .value_parser(parse_codec),
        )
        .arg(
            Arg::new("label")
                .short('L')
                .long("label")
                .value_name("LABEL")
                .help("Volume label (not implemented: the builder takes no volume name yet)"),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .help("No progress or warnings on stderr")
                .action(ArgAction::SetTrue),
        )
        .args(fs_core::cli::format_args())
        .after_help(
            "Examples:\n  \
             mkfs.erofs out.img ./rootfs\n  \
             mkfs.erofs -b 1024 out.img ./rootfs\n  \
             mkfs.erofs -z lz4hc,12 out.img ./rootfs\n  \
             mkfs.erofs out.img ./rootfs | jq '.skipped'\n  \
             mkfs.erofs --text -q out.img ./rootfs     print nothing on success",
        )
}

/// A block size in bytes, as `log2`: a power of two the format allows.
fn parse_block_size(text: &str) -> Result<u8, String> {
    let bytes: u64 = text
        .parse()
        .map_err(|e| format!("{text:?} is not a number of bytes: {e}"))?;
    let bits = bytes.trailing_zeros();
    if bytes.is_power_of_two() && u8::try_from(bits).is_ok_and(is_valid_blkszbits) {
        Ok(bits as u8)
    } else {
        Err(format!(
            "{bytes} is not a power of 2 in {MIN_BLOCK_SIZE}..={MAX_BLOCK_SIZE}"
        ))
    }
}

/// One source entry the builder left out, and why.
struct Skipped {
    path: PathBuf,
    reason: &'static str,
}

/// What the walk found: the tree to build, and what it left out.
#[derive(Default)]
struct Walk {
    skipped: Vec<Skipped>,
    files: u64,
    directories: u64,
    symlinks: u64,
    special_files: u64,
    /// Names beyond the first of an inode that has several.
    hard_links: u64,
    compression: Option<Codec>,
    /// The first path in the image of each source inode with several names,
    /// by device and inode number.
    linked: HashMap<(u64, u64), String>,
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let output = matches
        .get_one::<OsString>("output")
        .expect("clap requires the output");
    let source = matches
        .get_one::<OsString>("source")
        .expect("clap requires the source");
    let blkszbits = matches
        .get_one::<u8>("block-size")
        .copied()
        .unwrap_or(DEFAULT_BLOCK_SIZE.trailing_zeros() as u8);
    let quiet = matches.get_flag("quiet");

    // Refused before anything is read or written: a label asked for and
    // silently not written is a wrong image, not a smaller feature.
    if matches.get_one::<String>("label").is_some() {
        return Err(CliError::not_implemented(
            "--label: the EROFS image builder takes no volume name yet",
        ));
    }

    let source_path = Path::new(source);
    let compression = matches.get_one::<Codec>("compress").copied();
    let mut walk = Walk {
        compression,
        ..Walk::default()
    };
    let root = root_node(source_path, &mut walk)
        .map_err(|e| CliError::failed(format!("walking {}: {e}", source_path.display())))?;
    if !quiet {
        for s in &walk.skipped {
            eprintln!("warning: left out {} ({})", s.path.display(), s.reason);
        }
    }

    // LZ4HC is the builder's own compressor, one block to a pcluster as
    // mkfs.erofs makes them unless told otherwise. LZ4 is chosen file by
    // file, in `file_node`.
    let options = BuildOptions {
        compression: match compression {
            Some(Codec::Lz4Hc(level)) => Some(Compression {
                lz4hc_level: level,
                max_pcluster_bytes: 1u32 << blkszbits,
            }),
            Some(Codec::Lz4) | None => None,
        },
        ..BuildOptions::default()
    };
    let image = build_image_with(root, blkszbits, options)
        .map_err(|e| CliError::failed(format!("building the image: {e}")))?;
    let output_path = Path::new(output);
    std::fs::write(output_path, &image)
        .map_err(|e| CliError::failed(format!("writing {}: {e}", output_path.display())))?;

    // THE REPORT IS READ BACK, not restated: the superblock as written is
    // what a reader will see, so it is what the report says.
    let start = EROFS_SUPER_OFFSET as usize;
    let sb = image
        .get(start..start + EROFS_SUPER_BLOCK_SIZE)
        .ok_or_else(|| CliError::failed("the built image is shorter than its superblock"))
        .and_then(|bytes| {
            Superblock::parse(bytes)
                .map_err(|e| CliError::failed(format!("reading back the superblock: {e}")))
        })?;
    let block_size = sb.block_size();
    if !quiet {
        eprintln!(
            "wrote {} bytes to {} (block size {block_size}, {} blocks)",
            image.len(),
            output_path.display(),
            sb.blocks
        );
    }

    let report = Json::object([
        ("image", Json::from(output_path.display().to_string())),
        ("bytes", Json::from(image.len() as u64)),
        ("block_size", Json::from(block_size)),
        ("blocks", Json::from(sb.blocks)),
        ("inodes", Json::from(sb.inos)),
        ("uuid", Json::from(super::format_uuid(&sb.uuid))),
        ("files", Json::from(walk.files)),
        ("directories", Json::from(walk.directories)),
        ("symlinks", Json::from(walk.symlinks)),
        ("special_files", Json::from(walk.special_files)),
        ("hard_links", Json::from(walk.hard_links)),
        (
            "compression",
            compression.map_or(Json::Null, |codec| Json::from(codec.name())),
        ),
        (
            "skipped",
            Json::Arr(
                walk.skipped
                    .iter()
                    .map(|s| {
                        Json::object([
                            ("path", Json::from(s.path.display().to_string())),
                            ("reason", Json::from(s.reason)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ]);
    // `--text` keeps the tool's old stdout: nothing, on success.
    Ok(Outcome::report(report).with_text(""))
}

/// The image's root: SOURCE itself when it is a directory, or a root
/// holding SOURCE when it is a single regular file.
fn root_node(path: &Path, walk: &mut Walk) -> std::io::Result<Node<'static>> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        return walk_dir(path, "", &meta, walk);
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the source must be a directory or a regular file",
        ));
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let mut entries = BTreeMap::new();
    entries.insert(name, file_node(path, &meta, walk.compression)?);
    walk.files += 1;
    walk.directories += 1;
    Ok(Node::Dir {
        mode: DEFAULT_DIR_MODE,
        entries,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    })
}

/// The directory at `path`, which is `at` from the image's root.
fn walk_dir(
    path: &Path,
    at: &str,
    meta: &std::fs::Metadata,
    walk: &mut Walk,
) -> std::io::Result<Node<'static>> {
    walk.directories += 1;
    let mut entries = BTreeMap::new();
    // By name, so that which name of a hardlinked file is met first, and so
    // the image, does not depend on the order the host lists a directory in.
    let mut children = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(std::fs::DirEntry::file_name);
    for entry in children {
        let child = entry.path();
        // Not followed: a symlink is its own entry, not what it points at.
        let m = std::fs::symlink_metadata(&child)?;
        let Ok(name) = entry.file_name().into_string() else {
            walk.skipped.push(Skipped {
                path: child,
                reason: "name is not UTF-8",
            });
            continue;
        };
        let inside = if at.is_empty() {
            name.clone()
        } else {
            format!("{at}/{name}")
        };
        if m.is_dir() {
            entries.insert(name, walk_dir(&child, &inside, &m, walk)?);
            continue;
        }
        let identity = link_identity(&m);
        if let Some(target) = identity.and_then(|id| walk.linked.get(&id)) {
            let target = target.clone();
            entries.insert(name, Node::HardLink { target });
            walk.hard_links += 1;
            continue;
        }
        let node = if m.is_file() {
            walk.files += 1;
            Ok(file_node(&child, &m, walk.compression)?)
        } else if m.file_type().is_symlink() {
            symlink_node(&child, &m)?.inspect(|_| walk.symlinks += 1)
        } else {
            special_node(&m).inspect(|_| walk.special_files += 1)
        };
        match node {
            Ok(node) => {
                if let Some(id) = identity {
                    walk.linked.insert(id, inside);
                }
                entries.insert(name, node);
            }
            Err(reason) => walk.skipped.push(Skipped {
                path: child,
                reason,
            }),
        }
    }
    Ok(Node::Dir {
        mode: mode_for(meta, DEFAULT_DIR_MODE),
        entries,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    })
}

fn file_node(
    path: &Path,
    meta: &std::fs::Metadata,
    compression: Option<Codec>,
) -> std::io::Result<Node<'static>> {
    let mode = mode_for(meta, DEFAULT_FILE_MODE);
    let data = std::fs::read(path)?;
    // An empty file has nothing to compress.
    if compression == Some(Codec::Lz4) && !data.is_empty() {
        return Ok(Node::CompressedFile(CompressedFileSpec {
            mode,
            data,
            algo: CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
            index_format: CompressedFileSpec::default_index_format(),
            ztailpacking: false,
            target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
        }));
    }
    Ok(Node::File {
        mode,
        data,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    })
}

/// The symlink at `path`, its target as it is written, or why it is left out.
fn symlink_node(
    path: &Path,
    meta: &std::fs::Metadata,
) -> std::io::Result<Result<Node<'static>, &'static str>> {
    let Ok(target) = std::fs::read_link(path)?.into_os_string().into_string() else {
        return Ok(Err("symlink target is not UTF-8"));
    };
    // Windows keeps a target with its own separator; the image holds paths.
    #[cfg(windows)]
    let target = target.replace('\\', "/");
    Ok(Ok(Node::Symlink {
        mode: mode_for(meta, DEFAULT_SYMLINK_MODE),
        target,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }))
}

/// The device and inode number of a source entry that has several names.
#[cfg(unix)]
fn link_identity(m: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (m.nlink() > 1).then(|| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn link_identity(_m: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// A FIFO, socket or device node, or why it is left out.
#[cfg(unix)]
fn special_node(m: &std::fs::Metadata) -> Result<Node<'static>, &'static str> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let kind = m.file_type();
    let mode = m.mode() as u16;
    if kind.is_fifo() || kind.is_socket() {
        return Ok(Node::Special {
            mode,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        });
    }
    if kind.is_char_device() || kind.is_block_device() {
        let rdev = host_device_numbers(m.rdev())
            .and_then(|(major, minor)| image_device_number(major, minor))
            .ok_or("device number the image cannot hold")?;
        return Ok(Node::Device {
            mode,
            rdev,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        });
    }
    Err("special file")
}

#[cfg(not(unix))]
fn special_node(_m: &std::fs::Metadata) -> Result<Node<'static>, &'static str> {
    Err("special file")
}

/// The major and minor numbers in a host's `st_rdev`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn host_device_numbers(dev: u64) -> Option<(u64, u64)> {
    Some((
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
        (dev & 0xff) | ((dev >> 12) & !0xff),
    ))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn host_device_numbers(dev: u64) -> Option<(u64, u64)> {
    Some(((dev >> 24) & 0xff, dev & 0x00ff_ffff))
}

/// A Unix whose `st_rdev` this tool does not know how to read.
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn host_device_numbers(_dev: u64) -> Option<(u64, u64)> {
    None
}

/// Linux's 32-bit device number, which is what the image holds: 12 bits of
/// major and 20 of minor.
#[cfg(unix)]
fn image_device_number(major: u64, minor: u64) -> Option<u32> {
    if major >= 1 << 12 || minor >= 1 << 20 {
        return None;
    }
    Some(((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as u32)
}

/// The host's type and permission bits, which is what the reader looks
/// at.
#[cfg(unix)]
fn mode_for(m: &std::fs::Metadata, default: u16) -> u16 {
    use std::os::unix::fs::MetadataExt;
    match m.mode() as u16 {
        0 => default,
        mode => mode,
    }
}

#[cfg(not(unix))]
fn mode_for(_m: &std::fs::Metadata, default: u16) -> u16 {
    default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_is_spelled_as_erofs_utils_spells_it() {
        assert_eq!(parse_codec("lz4"), Ok(Codec::Lz4));
        assert_eq!(parse_codec("lz4hc"), Ok(Codec::Lz4Hc(9)));
        assert_eq!(parse_codec("lz4hc,1"), Ok(Codec::Lz4Hc(1)));
        assert_eq!(parse_codec("lz4hc,12"), Ok(Codec::Lz4Hc(12)));
        for refused in [
            "", "LZ4", "lz4,3", "lz4hc,", "lz4hc,0", "lz4hc,13", "lz4hc,x", "zstd", "deflate",
        ] {
            assert!(parse_codec(refused).is_err(), "{refused:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_device_number_is_written_as_linux_encodes_it() {
        assert_eq!(image_device_number(1, 3), Some(0x0103));
        assert_eq!(image_device_number(8, 1), Some(0x0801));
        assert_eq!(image_device_number(259, 0x12345), Some(0x1231_0345));
        assert_eq!(image_device_number(0xfff, 0xf_ffff), Some(u32::MAX));
        assert_eq!(image_device_number(1 << 12, 0), None);
        assert_eq!(image_device_number(0, 1 << 20), None);
    }

    /// A real device node's metadata, which a test cannot make for itself.
    #[cfg(target_os = "linux")]
    #[test]
    fn dev_null_is_a_character_device_numbered_1_3() {
        let meta = std::fs::symlink_metadata("/dev/null").unwrap();
        match special_node(&meta) {
            Ok(Node::Device { mode, rdev, .. }) => {
                assert_eq!(mode & 0o170000, 0o020000, "a character device");
                assert_eq!(rdev, 0x0103);
            }
            other => panic!("/dev/null became {other:?}"),
        }
    }
}
