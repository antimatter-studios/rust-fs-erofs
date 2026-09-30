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
//! Uncompressed, compact inodes, FLAT_PLAIN. Symlinks and special files
//! are left out with a warning: the library can write them, and exposing
//! that here is a follow-up. A hardlinked file is copied under each of its
//! names, as the builder always did.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_erofs::mkfs::{build_image, Node, NodeMeta, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE};
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

fn command() -> Cmd {
    Cmd::new("mkfs.erofs")
        .about("Build an EROFS image from a directory")
        .long_about(
            "Build an uncompressed EROFS image of SOURCE, a directory (or a single regular \
             file, which becomes the image's only entry), and write it to OUTPUT.\n\n\
             Regular files and directories are copied with their permission bits; a \
             hardlinked file is copied under each of its names. Symlinks, special files \
             and names that are not UTF-8 are left out, each named in the report and \
             warned about on stderr.\n\n\
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
struct Walk {
    skipped: Vec<Skipped>,
    files: u64,
    directories: u64,
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
    let mut walk = Walk {
        skipped: Vec::new(),
        files: 0,
        directories: 0,
    };
    let root = root_node(source_path, &mut walk)
        .map_err(|e| CliError::failed(format!("walking {}: {e}", source_path.display())))?;
    if !quiet {
        for s in &walk.skipped {
            eprintln!("warning: left out {} ({})", s.path.display(), s.reason);
        }
    }

    let image = build_image(root, blkszbits)
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
fn root_node(path: &Path, walk: &mut Walk) -> std::io::Result<Node> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        return walk_dir(path, &meta, walk);
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
    entries.insert(name, file_node(path, &meta)?);
    walk.files += 1;
    walk.directories += 1;
    Ok(Node::Dir {
        mode: DEFAULT_DIR_MODE,
        entries,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    })
}

fn walk_dir(path: &Path, meta: &std::fs::Metadata, walk: &mut Walk) -> std::io::Result<Node> {
    walk.directories += 1;
    let mut entries = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
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
        if m.is_dir() {
            entries.insert(name, walk_dir(&child, &m, walk)?);
        } else if m.is_file() {
            entries.insert(name, file_node(&child, &m)?);
            walk.files += 1;
        } else {
            walk.skipped.push(Skipped {
                path: child,
                reason: if m.file_type().is_symlink() {
                    "symlink"
                } else {
                    "special file"
                },
            });
        }
    }
    Ok(Node::Dir {
        mode: mode_for(meta, DEFAULT_DIR_MODE),
        entries,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    })
}

fn file_node(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<Node> {
    Ok(Node::File {
        mode: mode_for(meta, DEFAULT_FILE_MODE),
        data: std::fs::read(path)?,
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    })
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
