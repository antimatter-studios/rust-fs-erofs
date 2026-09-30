//! `fs.erofs <target> <verb>`: an errand inside an EROFS image, without
//! mounting it.
//!
//! The verbs are the shared set every `fs.<fs>` answers: `ls`, `read`,
//! `get`/`info`, and `write`, `mkdir`, `set`, `resize`. EROFS is a
//! read-only format, so the last four exist and refuse, with exit status 3
//! and "EROFS is read-only": a script moved here from a writable
//! filesystem fails loudly instead of meaning something else.
//!
//! Metadata is JSON (or `--text`); file content is raw bytes. Every layout
//! the library reads is read here, compressed ones included.

use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_core::{BlockRead, FileDevice, OwnedSlice};
use fs_erofs::{FileType, Filesystem, Inode, InodeVersion};

pub const TOOL: Tool = Tool {
    name: "fs.erofs",
    verb: "fs",
    section: 1,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "List, read and inspect an EROFS image without mounting it",
    command,
    run,
};

/// The canonical keys every `fs.<fs>` answers, in the shared order.
/// EROFS's own are nested under `erofs`.
pub const KEYS: &[&str] = &[
    "fs",
    "label",
    "total_bytes",
    "free_bytes",
    "block_size",
    "dirty",
    "erofs",
];

/// What every verb that would change the image answers.
const READ_ONLY: &str = "EROFS is read-only";

fn command() -> Cmd {
    Cmd::new("fs.erofs")
        .about("List, read and inspect an EROFS image without mounting it")
        .long_about(
            "Work inside an EROFS image directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, see what is in it, check how \
             it was built), not a place to do real filesystem work: for that, mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` writes the file's raw \
             bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
             exit status: 1 failed, 2 wrong command line, 3 EROFS cannot do that (it is \
             read-only).",
        )
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("The image file or device")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help("Where the filesystem starts in TARGET, for one embedded in a larger image")
                .value_parser(value_parser!(u64))
                .global(true),
        )
        .args(fs_core::cli::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about("List a directory: name, type, size, mode, mtime (and a symlink's target)")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs ls /etc\n  \
                     fs.erofs rootfs.erofs ls / | jq -r '.[].name'\n  \
                     fs.erofs rootfs.erofs ls /bin/sh | jq -r '.[0].target'\n  \
                     fs.erofs rootfs.erofs ls --text /",
                ),
        )
        .subcommand(
            Cmd::new("read")
                .about("Write a file's bytes to stdout, or to a file with -o")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .help("Write here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs read /etc/hostname\n  \
                     fs.erofs rootfs.erofs read /var/log/syslog | grep -i error\n  \
                     fs.erofs rootfs.erofs read /backup.tar -o backup.tar",
                ),
        )
        .subcommand(
            Cmd::new("write")
                .about("Refused: EROFS is read-only")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs write /etc/hostname < hostname\n\n\
                     Answers `EROFS is read-only` (exit 3) and reads nothing: rebuild the \
                     image with mkfs.erofs instead.",
                ),
        )
        .subcommand(
            Cmd::new("mkdir")
                .about("Refused: EROFS is read-only")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs mkdir /backup\n\n\
                     Answers `EROFS is read-only` (exit 3).",
                ),
        )
        .subcommand(key_command(
            "get",
            "Report the filesystem's properties, or one of them",
        ))
        .subcommand(key_command(
            "info",
            "The same as get: every property, or one of them",
        ))
        .subcommand(
            Cmd::new("set")
                .about("Refused: EROFS is read-only")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs set label BACKUP\n\n\
                     Answers `EROFS is read-only` (exit 3). A label is set when an image is \
                     built, and `mkfs.erofs` cannot set one yet.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Refused: EROFS is read-only")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Do it without asking"),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs resize 1G --force\n\n\
                     Answers `EROFS is read-only` (exit 3).",
                ),
        )
        .after_help(
            "Examples:\n  fs.erofs rootfs.erofs ls /\n  \
             fs.erofs rootfs.erofs read /etc/fstab > fstab\n  \
             fs.erofs rootfs.erofs get erofs.uuid --text\n  \
             fs.erofs --offset 1048576 firmware.bin info",
        )
}

fn key_command(name: &'static str, about: &'static str) -> Cmd {
    Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("key")
                .value_name("KEY")
                .help(format!("One of: {} (or erofs.<field>)", KEYS.join(", "))),
        )
        .after_help(format!(
            "Examples:\n  fs.erofs rootfs.erofs {name}\n  \
             fs.erofs rootfs.erofs {name} block_size --text\n  \
             fs.erofs rootfs.erofs {name} erofs.inode_count"
        ))
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    let offset = sub
        .get_one::<u64>("offset")
        .or_else(|| matches.get_one::<u64>("offset"))
        .copied()
        .unwrap_or(0);
    match verb {
        "ls" => ls(&open(target, offset)?, path_arg(sub)),
        "read" => read(&open(target, offset)?, path_arg(sub), sub.get_one("output")),
        "get" | "info" => get(
            &open(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        // Refused before the image is opened, and `write` before stdin is
        // read: nothing about the target can change the answer.
        "write" | "mkdir" | "set" | "resize" => Err(CliError::refused(format!(
            "{verb}: {READ_ONLY}; build a new image with mkfs.erofs instead"
        ))),
        other => unreachable!("clap knows no verb {other}"),
    }
}

fn path_arg(sub: &ArgMatches) -> &[u8] {
    let path = sub
        .get_one::<OsString>("path")
        .expect("clap requires or defaults the path");
    os_bytes(path)
}

#[cfg(unix)]
fn os_bytes(s: &OsString) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(s: &OsString) -> &[u8] {
    s.to_str().map(str::as_bytes).unwrap_or_default()
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Open `target` read-only, `offset` bytes in, as an EROFS image.
fn open(target: &OsString, offset: u64) -> Result<Filesystem, CliError> {
    let name = target.to_string_lossy();
    let file = FileDevice::open(std::path::Path::new(target))
        .map_err(|e| CliError::failed(format!("open {name}: {e}")))?;
    let size = file.size_bytes();
    let dev: Arc<dyn BlockRead> = if offset == 0 {
        Arc::new(file)
    } else if offset >= size {
        return Err(CliError::failed(format!(
            "--offset {offset} is past the end of {name} ({size} bytes)"
        )));
    } else {
        Arc::new(OwnedSlice::new(Arc::new(file), offset, size - offset))
    };
    Filesystem::open(dev)
        .map_err(|e| CliError::failed(format!("{name} is not a readable EROFS image: {e}")))
}

fn erofs_error(what: &[u8], e: fs_erofs::Error) -> CliError {
    CliError::failed(format!("{}: {e}", show(what)))
}

fn type_name(t: FileType) -> &'static str {
    match t {
        FileType::RegularFile => "file",
        FileType::Dir => "dir",
        FileType::Symlink => "symlink",
        FileType::ChrDev => "char",
        FileType::BlkDev => "block",
        FileType::Fifo => "fifo",
        FileType::Sock => "socket",
        FileType::Unknown => "unknown",
    }
}

fn type_char(t: &str) -> char {
    match t {
        "file" => '-',
        "dir" => 'd',
        "symlink" => 'l',
        "char" => 'c',
        "block" => 'b',
        "fifo" => 'p',
        "socket" => 's',
        _ => '?',
    }
}

/// One `ls` entry: the fields every `fs.<fs>` reports, typed the same way
/// everywhere -- name (string), type (string), size (number), mode (octal
/// string), mtime (seconds since the epoch, number), inode (number), and
/// target (string) for a symlink. A name that is not UTF-8 is shown
/// lossily, with its exact bytes in `name_hex`. A device carries its
/// `major` and `minor` too, which is what `ls -l` shows in place of a size.
///
/// A compact inode stores no time of its own: the kernel gives it the
/// image's build time, and so does this.
fn entry(fs: &Filesystem, name: &[u8], inode: &Inode) -> Result<Json, fs_erofs::Error> {
    let kind = inode.file_type();
    let mut fields = vec![("name", Json::from(show(name)))];
    if std::str::from_utf8(name).is_err() {
        fields.push((
            "name_hex",
            Json::from(name.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    let mtime = match inode.format.version {
        InodeVersion::Compact => fs.superblock().build_time,
        InodeVersion::Extended => inode.mtime,
    };
    fields.extend([
        ("type", Json::from(type_name(kind))),
        ("size", Json::from(inode.size)),
        ("mode", Json::from(format!("{:04o}", inode.mode & 0o7777))),
        ("mtime", Json::from(mtime)),
        ("inode", Json::from(inode.nid)),
    ]);
    if kind == FileType::Symlink {
        fields.push(("target", Json::from(show(&fs.read_symlink_target(inode)?))));
    }
    if let Some((major, minor)) = inode.rdev() {
        fields.extend([("major", Json::from(major)), ("minor", Json::from(minor))]);
    }
    Ok(Json::object(fields))
}

fn entry_text(e: &Json) -> String {
    let field = |k: &str| e.get(k).map(Json::to_text).unwrap_or_default();
    let mut line = format!(
        "{}{} {:>12} {}",
        type_char(&field("type")),
        field("mode"),
        field("size"),
        field("name")
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    line
}

fn ls(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| erofs_error(path, e))?;
    let entries = if inode.is_dir() {
        let mut listed = Vec::new();
        for d in fs.read_dir(&inode).map_err(|e| erofs_error(path, e))? {
            if d.name == b"." || d.name == b".." {
                continue;
            }
            let mut full = path.to_vec();
            if !full.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(&d.name);
            let child = fs.read_inode(d.nid).map_err(|e| erofs_error(&full, e))?;
            listed.push(entry(fs, &d.name, &child).map_err(|e| erofs_error(&full, e))?);
        }
        // The directory's own order is by name hash; a listing is by name.
        listed.sort_by(|a, b| {
            a.get("name")
                .map(Json::to_text)
                .cmp(&b.get("name").map(Json::to_text))
        });
        listed
    } else {
        let name = path
            .rsplit(|b| *b == b'/')
            .find(|s| !s.is_empty())
            .unwrap_or(path);
        vec![entry(fs, name, &inode).map_err(|e| erofs_error(path, e))?]
    };
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome::report(Json::Arr(entries)).with_text(text))
}

/// Stream a regular file's bytes. Each chunk is read before it is
/// written, so a file whose blocks turn out unreadable part-way stops with
/// status 1; everything that can be refused up front (no such path, a
/// directory, a symlink, a device) is refused before a byte is written.
/// `-o FILE` writes `FILE.partial` and renames it, so FILE is never left
/// half written.
fn read(fs: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| erofs_error(path, e))?;
    match inode.file_type() {
        FileType::RegularFile => {}
        FileType::Dir => {
            return Err(CliError::failed(format!("{}: is a directory", show(path))));
        }
        FileType::Symlink => {
            let target = fs.read_symlink_target(&inode).unwrap_or_default();
            return Err(CliError::failed(format!(
                "{}: is a symlink to {}; read the target instead",
                show(path),
                show(&target)
            )));
        }
        _ => {
            return Err(CliError::failed(format!(
                "{}: not a regular file",
                show(path)
            )));
        }
    }
    const CHUNK: usize = 1 << 20;
    let size = inode.size;
    let mut buf = vec![0u8; CHUNK.min(size.try_into().unwrap_or(CHUNK))];
    let mut copy = |sink: &mut dyn Write| -> Result<(), CliError> {
        let mut offset = 0u64;
        while offset < size {
            let want = buf
                .len()
                .min((size - offset).try_into().unwrap_or(usize::MAX));
            fs.read_file(&inode, offset, &mut buf[..want])
                .map_err(|e| erofs_error(path, e))?;
            let got = want;
            sink.write_all(&buf[..got])
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            offset += got as u64;
        }
        sink.flush()
            .map_err(|e| CliError::failed(format!("write: {e}")))
    };
    match output {
        None => copy(&mut std::io::stdout().lock())?,
        Some(file) => {
            let dest = std::path::Path::new(file);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let mut f = std::fs::File::create(&partial)
                .map_err(|e| CliError::failed(format!("create {}: {e}", partial.display())))?;
            if let Err(e) = copy(&mut f) {
                drop(f);
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, dest)
                .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))?;
        }
    }
    Ok(Outcome::done())
}

/// The envelope: the shared keys first, EROFS's own under `erofs`. An
/// image is written once and never modified, so nothing is free and
/// nothing is dirty; a label is the superblock's volume name, null when
/// the image has none.
pub fn envelope(fs: &Filesystem) -> Json {
    let sb = fs.superblock();
    let name_end = sb
        .volume_name
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(sb.volume_name.len());
    let label = &sb.volume_name[..name_end];
    Json::object([
        ("fs", Json::from("erofs")),
        (
            "label",
            if label.is_empty() {
                Json::Null
            } else {
                Json::from(show(label))
            },
        ),
        (
            "total_bytes",
            Json::from(u64::from(sb.blocks) * sb.block_size()),
        ),
        ("free_bytes", Json::from(0u64)),
        ("block_size", Json::from(sb.block_size())),
        ("dirty", Json::from(false)),
        (
            "erofs",
            Json::object([
                ("uuid", Json::from(super::format_uuid(&sb.uuid))),
                ("inode_count", Json::from(sb.inos)),
                ("total_blocks", Json::from(sb.blocks)),
                ("build_time", Json::from(sb.build_time)),
                ("build_time_nsec", Json::from(sb.build_time_nsec)),
                ("root_nid", Json::from(sb.root_nid)),
                ("feature_compat", Json::from(sb.feature_compat)),
                ("feature_incompat", Json::from(sb.feature_incompat)),
                ("extra_devices", Json::from(sb.extra_devices)),
            ]),
        ),
    ])
}

fn get(fs: &Filesystem, key: Option<&str>) -> Result<Outcome, CliError> {
    let all = envelope(fs);
    let Some(key) = key else {
        return Ok(Outcome::report(all));
    };
    let mut value = Some(&all);
    for part in key.split('.') {
        value = value.and_then(|v| v.get(part));
    }
    let Some(value) = value else {
        return Err(CliError::usage(format!(
            "no key {key:?}; the keys are {} (and erofs.<field>)",
            KEYS.join(", ")
        )));
    };
    let text = value.to_text();
    Ok(Outcome::report(Json::object([(key, value.clone())])).with_text(text))
}
