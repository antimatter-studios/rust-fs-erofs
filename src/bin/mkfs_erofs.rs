//! mkfs.erofs — standalone CLI for creating EROFS filesystem images.
//!
//! Usage:
//!
//!   mkfs_erofs OUTPUT SOURCE_DIR [--block-size 4096]
//!
//! Walks SOURCE_DIR, builds an EROFS image (Phase 0: uncompressed,
//! compact inodes, FLAT_PLAIN, no xattrs), writes to OUTPUT.
//!
//! Independent implementation, written from `linux/fs/erofs/erofs_fs.h`.
//! Not derived from any GPL'd EROFS codebase.

use fs_erofs::mkfs::{build_image, Node, NodeMeta, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: mkfs_erofs OUTPUT SOURCE_DIR [--block-size N]

Phase 0: uncompressed, compact inodes, FLAT_PLAIN. Symlinks, hardlinks,
and special files are skipped (warned to stderr).

Options:
  --block-size N    Block size in bytes. Power of 2, 512..=65536.
                    Default: 4096.
  -h, --help        This help.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut block_size: u64 = 4096;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::from(0);
            }
            "--block-size" => {
                if i + 1 >= args.len() {
                    eprintln!("--block-size needs an argument");
                    return ExitCode::from(2);
                }
                block_size = match args[i + 1].parse::<u64>() {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("--block-size: {e}");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            s if s.starts_with('-') => {
                eprintln!("unknown flag: {s}");
                return ExitCode::from(2);
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    if positional.len() != 2 {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    }
    let blkszbits = match block_size_to_bits(block_size) {
        Some(b) => b,
        None => {
            eprintln!("--block-size must be a power of 2 in 512..=65536");
            return ExitCode::from(2);
        }
    };

    let out = &positional[0];
    let src = &positional[1];

    let root = match walk(Path::new(src)) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("walking {src}: {e}");
            return ExitCode::from(1);
        }
    };

    let img = match build_image(root, blkszbits) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("build_image: {e}");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = std::fs::write(out, &img) {
        eprintln!("writing {out}: {e}");
        return ExitCode::from(1);
    }

    eprintln!(
        "wrote {} bytes to {out} (block size {}, {} blocks)",
        img.len(),
        block_size,
        img.len() as u64 / block_size,
    );
    ExitCode::from(0)
}

fn block_size_to_bits(bs: u64) -> Option<u8> {
    if !bs.is_power_of_two() {
        return None;
    }
    let bits = bs.trailing_zeros() as u8;
    if (9..=16).contains(&bits) {
        Some(bits)
    } else {
        None
    }
}

/// Walk a host directory and produce a `Node` tree. Symlinks + special
/// files print a stderr note and are dropped.
fn walk(path: &Path) -> std::io::Result<Node> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        for ent in std::fs::read_dir(path)? {
            let ent = ent?;
            let name = match ent.file_name().into_string() {
                Ok(s) => s,
                Err(_) => {
                    eprintln!("warning: skipping non-UTF-8 entry {:?}", ent.file_name());
                    continue;
                }
            };
            let m = ent.metadata()?;
            if m.is_dir() {
                entries.insert(name, walk(&ent.path())?);
            } else if m.is_file() {
                let data = std::fs::read(ent.path())?;
                entries.insert(
                    name,
                    Node::File {
                        mode: mode_for(&m, DEFAULT_FILE_MODE),
                        data,
                        meta: NodeMeta::default(),
                        xattrs: Vec::new(),
                    },
                );
            } else {
                eprintln!(
                    "warning: skipping {} (not a regular file or directory)",
                    ent.path().display()
                );
            }
        }
        Ok(Node::Dir {
            mode: mode_for(&meta, DEFAULT_DIR_MODE),
            entries,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        })
    } else if meta.is_file() {
        // Caller passed a regular file as SOURCE_DIR -- wrap as a single-
        // file root dir.
        let mut entries: BTreeMap<String, Node> = BTreeMap::new();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let data = std::fs::read(path)?;
        entries.insert(
            name,
            Node::File {
                mode: mode_for(&meta, DEFAULT_FILE_MODE),
                data,
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        );
        Ok(Node::Dir {
            mode: DEFAULT_DIR_MODE,
            entries,
            meta: NodeMeta::default(),
            xattrs: Vec::new(),
        })
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source must be a directory or regular file",
        ))
    }
}

#[cfg(unix)]
fn mode_for(m: &std::fs::Metadata, default: u16) -> u16 {
    use std::os::unix::fs::MetadataExt;
    let host_mode = m.mode() as u16;
    // Combine host's S_IF* type bits + perms; reader only inspects
    // the type nibble and perms.
    if host_mode == 0 {
        default
    } else {
        host_mode
    }
}

#[cfg(not(unix))]
fn mode_for(_m: &std::fs::Metadata, default: u16) -> u16 {
    default
}
