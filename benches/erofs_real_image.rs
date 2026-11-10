//! Build one image from an extracted Android EROFS tree with the selected
//! compile-time LZ4HC backend. Run through scripts/bench-real-erofs-lz4hc.sh.

use fs_erofs::mkfs::{
    build_image_to, BuildOptions, Compression, Content, Node, NodeMeta, XattrSpec,
};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::error::Error;
use std::fs::{self, File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;
const S_IFIFO: u32 = 0o010000;
const S_IFSOCK: u32 = 0o140000;

#[derive(Default)]
struct TreeStats {
    files: u64,
    directories: u64,
    input_bytes: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let (input, output) = arguments()?;
    let mut hardlinks = HashMap::new();
    let mut stats = TreeStats::default();
    let prepare_started = Instant::now();
    let root = load_node(&input, Path::new(""), &mut hardlinks, &mut stats)?;
    let prepare_seconds = prepare_started.elapsed().as_secs_f64();
    let codec = if cfg!(feature = "bench-liblz4-backend") {
        "liblz4"
    } else {
        "lz4_flex"
    };
    let erofs_revision = env::var("EROFS_REV").unwrap_or_else(|_| git_revision("."));
    let lz4_flex_revision = env::var("LZ4_FLEX_REV").unwrap_or_else(|_| "Cargo.lock".into());

    let options = BuildOptions {
        compression: Some(Compression::LZ4HC_12_32K),
        share_xattrs: true,
        ..BuildOptions::default()
    };
    let mut image = File::create(&output)?;
    let started = Instant::now();
    let image_bytes = build_image_to(root, 12, options, &mut image)?;
    image.set_len(image_bytes)?;
    let elapsed = started.elapsed();

    println!(
        "real_image,codec={codec},erofs_rev={erofs_revision},lz4_flex_rev={lz4_flex_revision},files={},directories={},input_bytes={},image_bytes={image_bytes},prepare_seconds={prepare_seconds:.6},build_seconds={:.6},mib_per_second={:.3},output={}",
        stats.files,
        stats.directories,
        stats.input_bytes,
        elapsed.as_secs_f64(),
        stats.input_bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64(),
        output.display(),
    );
    Ok(())
}

fn arguments() -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let mut input = None;
    let mut output = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--input-dir") => input = args.next().map(PathBuf::from),
            Some("--output") => output = args.next().map(PathBuf::from),
            _ => return Err("usage: erofs_real_image --input-dir DIR --output IMAGE".into()),
        }
    }
    let input = input.ok_or("--input-dir is required")?;
    let output = output.ok_or("--output is required")?;
    if !input.is_dir() {
        return Err(format!("{} is not a directory", input.display()).into());
    }
    Ok((input, output))
}

fn load_node(
    path: &Path,
    relative: &Path,
    hardlinks: &mut HashMap<(u64, u64), String>,
    stats: &mut TreeStats,
) -> Result<Node<'static>, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path)?;
    let mode = metadata.mode();
    let node_meta = node_meta(&metadata)?;
    let xattrs = read_xattrs(path)?;
    match mode & S_IFMT {
        S_IFDIR => {
            stats.directories += 1;
            let mut entries = BTreeMap::new();
            let mut children = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| format!("non-UTF-8 path under {}", path.display()))?;
                let child_relative = relative.join(name);
                let child_node = load_node(&child.path(), &child_relative, hardlinks, stats)?;
                entries.insert(name.to_owned(), child_node);
            }
            Ok(Node::Dir {
                mode: mode as u16,
                entries,
                meta: node_meta,
                xattrs,
            })
        }
        S_IFREG => {
            stats.files += 1;
            stats.input_bytes += metadata.len();
            if metadata.nlink() > 1 {
                let key = (metadata.dev(), metadata.ino());
                if let Some(target) = hardlinks.get(&key) {
                    return Ok(Node::HardLink {
                        target: target.clone(),
                    });
                }
                hardlinks.insert(
                    key,
                    relative
                        .to_str()
                        .ok_or_else(|| format!("non-UTF-8 path: {}", relative.display()))?
                        .to_owned(),
                );
            }
            let source = path.to_path_buf();
            Ok(Node::StreamedFile {
                mode: mode as u16,
                content: Content::new(metadata.len(), move || File::open(source)),
                meta: node_meta,
                xattrs,
            })
        }
        S_IFLNK => {
            let target = fs::read_link(path)?;
            let target = target
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 symlink target at {}", path.display()))?;
            Ok(Node::Symlink {
                mode: mode as u16,
                target: target.to_owned(),
                meta: node_meta,
                xattrs,
            })
        }
        S_IFCHR | S_IFBLK => Ok(Node::Device {
            mode: mode as u16,
            rdev: metadata.rdev() as u32,
            meta: node_meta,
            xattrs,
        }),
        S_IFIFO | S_IFSOCK => Ok(Node::Special {
            mode: mode as u16,
            meta: node_meta,
            xattrs,
        }),
        kind => Err(format!("unsupported file type {kind:o} at {}", path.display()).into()),
    }
}

fn node_meta(metadata: &Metadata) -> Result<NodeMeta, Box<dyn Error>> {
    Ok(NodeMeta {
        uid: metadata.uid(),
        gid: metadata.gid(),
        mtime: u64::try_from(metadata.mtime())?,
        mtime_nsec: u32::try_from(metadata.mtime_nsec())?,
    })
}

fn read_xattrs(path: &Path) -> Result<Vec<XattrSpec>, Box<dyn Error>> {
    xattr::list(path)?
        .map(|name| {
            let full_name = name
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 xattr name on {}", path.display()))?;
            let value = xattr::get(path, &name)?
                .ok_or_else(|| format!("xattr disappeared on {}", path.display()))?;
            XattrSpec::from_full_name(full_name, value).map_err(Into::into)
        })
        .collect()
}

fn git_revision(path: &str) -> String {
    std::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
