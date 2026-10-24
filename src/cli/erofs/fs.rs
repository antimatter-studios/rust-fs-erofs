//! `fs.erofs <target> <verb>`: an errand inside an EROFS image, without
//! mounting it.
//!
//! The verbs are the shared set every `fs.<fs>` answers: `ls`, `stat`, `read`, `export`, `xattr`,
//! `get`/`info`, and `import`, `write`, `mkdir`, `rm`, `rmdir`, `mv`, `ln`, `truncate`,
//! `set`, `resize`. EROFS is a read-only format, so mutations refuse with exit status 3
//! and "EROFS is read-only": a script moved here from a writable
//! filesystem fails loudly instead of meaning something else.
//!
//! Metadata is JSON (or `--text`); file content is raw bytes. Every layout
//! the library reads is read here, compressed ones included.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{transfer, CliError, Json, Outcome, Tool};
use fs_core::{BlockRead, FileDevice, OwnedSlice};
use fs_erofs::{DataLayout, FileType, Filesystem, Inode, InodeVersion};

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
             Metadata is JSON on stdout (--text for people); `read` and `xattr PATH NAME` \
             write raw bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
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
                    Arg::new("recursive")
                        .long("recursive")
                        .short('R')
                        .action(ArgAction::SetTrue)
                        .help("Walk depth first, sorting names as bytes; never follow symlinks"),
                )
                .arg(
                    Arg::new("max-depth")
                        .long("max-depth")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help(
                            "Maximum recursive depth (default: 64); direct children have depth 1",
                        ),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help("Maximum recursive entries (default: 100000)"),
                )
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
                     fs.erofs rootfs.erofs ls --text /\n  \
                     fs.erofs rootfs.erofs ls -R --max-depth 8 --limit 1000 /etc\n\n\
                     Recursive entries add volume-absolute path and depth. The starting \
                     directory is omitted; a starting file or symlink has depth 0. A repeated \
                     directory inode is listed with revisited: true and not entered. If another \
                     entry would exceed either bound, or a directory cannot be read, the \
                     collected prefix stays on stdout and an error names the bound or path \
                     on stderr (exit 1). Exact fits succeed; zero allows no entries or depth.",
                ),
        )
        .subcommand(
            Cmd::new("stat")
                .about("Report one path: identity, type, mode, owner, links, sizes and times")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs stat /etc/hostname\n  \
                     fs.erofs rootfs.erofs stat /var/log | jq .nlink\n  \
                     fs.erofs rootfs.erofs stat --text /bin/sh\n\n\
                     A symlink is reported, not followed. Times are signed seconds since \
                     the epoch plus nanoseconds. Compact inodes use the image's build time; \
                     extended inodes store mtime. Access, change and birth times are not \
                     recorded and are null. Allocated bytes use the kernel's block accounting.",
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
            Cmd::new("export")
                .about("Copy a regular file, or with --recursive a directory tree")
                .args(transfer::export_args_with_times())
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs export /backup.tar backup.tar\n  \
                     fs.erofs rootfs.erofs export --replace /etc/hostname hostname\n  \
                     fs.erofs rootfs.erofs export /backup.tar - | tar tf -\n  \
                     fs.erofs rootfs.erofs export --recursive /etc etc\n  \
                     fs.erofs rootfs.erofs export --recursive --preserve=links,times /etc etc\n\n\
                     Files are copied in fixed chunks. A local destination is published only \
                     when complete; an existing name is refused unless --replace is given. \
                     The report includes the byte count and SHA-256; DEST - writes only raw \
                     bytes to stdout. Without --recursive directories are refused.\n\n\
                     Recursive export uses the shared tree transfer and reports every item. \
                     Existing directories are merged into; existing files require --replace. \
                     --max-depth bounds the walk. Special files are skipped. Symlinks require \
                     --preserve=links, which recreates opaque targets without following them \
                     and copies each regular inode once. --preserve=times sets modification \
                     times from extended inodes or the build time for compact inodes; access \
                     times are unavailable and reported as such. Symlink times are not set. \
                     --verify reads each staged file back before publication.",
                ),
        )
        .subcommand(
            refused_command(
                "import",
                "fs.erofs rootfs.erofs import hostname /etc/hostname\n  \
                 fs.erofs rootfs.erofs import - /backup.tar < backup.tar\n  \
                 fs.erofs rootfs.erofs import --replace hostname /etc/hostname",
            )
            .long_about(
                "Refused: EROFS is read-only. Import answers exit 3 before opening \
                 the image or source file, or reading stdin.",
            )
            .arg(
                Arg::new("source")
                    .value_name("SOURCE")
                    .required(true)
                    .value_parser(value_parser!(OsString))
                    .help("The local source file, or - for stdin; neither is read"),
            )
            .arg(mutation_path())
            .arg(
                Arg::new("replace")
                    .long("replace")
                    .action(ArgAction::SetTrue)
                    .help("Ask to replace PATH; EROFS still refuses import"),
            ),
        )
        .subcommand(
            Cmd::new("xattr")
                .about("List a path's extended attributes, or write one value's raw bytes")
                .arg(mutation_path())
                .arg(
                    Arg::new("name")
                        .value_name("NAME")
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .requires("name")
                        .help("Write the named attribute's value here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.erofs rootfs.erofs xattr /etc/hostname\n  \
                     fs.erofs rootfs.erofs xattr --text /etc/hostname\n  \
                     fs.erofs rootfs.erofs xattr /bin/tool security.capability -o caps.bin\n\n\
                     Without NAME, report name and size, sorted by name bytes. Non-UTF-8 \
                     names are shown lossily with exact bytes in name_hex. --text lists \
                     size, a tab and name. With NAME, write raw bytes even with --text; \
                     an empty value succeeds, a missing attribute fails (exit 1). A symlink \
                     is inspected without following it. -o uses FILE.partial and renames \
                     on success, as read does. Attributes cannot be set or removed.",
                ),
        )
        .subcommand(
            refused_command(
                "write",
                "fs.erofs rootfs.erofs write /etc/hostname < hostname\n  \
                 fs.erofs rootfs.erofs write /log --at 1024 < chunk",
            )
            .arg(mutation_path())
            .arg(
                Arg::new("at")
                    .long("at")
                    .value_name("BYTES")
                    .value_parser(value_parser!(u64))
                    .help("Position inside the file, not the filesystem's --offset in TARGET"),
            ),
        )
        .subcommand(
            refused_command(
                "mkdir",
                "fs.erofs rootfs.erofs mkdir /backup\n  \
                 fs.erofs rootfs.erofs mkdir --parents /backup/2026",
            )
            .arg(mutation_path())
            .arg(
                Arg::new("parents")
                    .long("parents")
                    .short('p')
                    .action(ArgAction::SetTrue)
                    .help("Create missing parent directories too"),
            ),
        )
        .subcommand(
            refused_command("rm", "fs.erofs rootfs.erofs rm /notes.txt").arg(mutation_path()),
        )
        .subcommand(
            refused_command("rmdir", "fs.erofs rootfs.erofs rmdir /backup").arg(mutation_path()),
        )
        .subcommand(
            refused_command(
                "mv",
                "fs.erofs rootfs.erofs mv /notes.txt /archive/notes.txt",
            )
            .arg(
                Arg::new("source")
                    .value_name("SOURCE")
                    .required(true)
                    .value_parser(value_parser!(OsString)),
            )
            .arg(mutation_path()),
        )
        .subcommand(
            refused_command(
                "ln",
                "fs.erofs rootfs.erofs ln /notes.txt /also-notes.txt\n  \
                 fs.erofs rootfs.erofs ln -s ../etc/hostname /hostname",
            )
            .arg(
                Arg::new("symbolic")
                    .short('s')
                    .long("symbolic")
                    .action(ArgAction::SetTrue)
                    .help("Make a symbolic link whose target is TARGET, as given"),
            )
            .arg(
                Arg::new("target")
                    .value_name("TARGET")
                    .required(true)
                    .value_parser(value_parser!(OsString)),
            )
            .arg(mutation_path()),
        )
        .subcommand(
            refused_command(
                "truncate",
                "fs.erofs rootfs.erofs truncate /log.txt 0\n  \
                 fs.erofs rootfs.erofs truncate /sparse.img 1073741824",
            )
            .arg(mutation_path())
            .arg(
                Arg::new("size")
                    .value_name("BYTES")
                    .required(true)
                    .value_parser(value_parser!(u64)),
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

fn refused_command(name: &'static str, examples: &'static str) -> Cmd {
    Cmd::new(name)
        .about("Refused: EROFS is read-only")
        .after_help(format!(
            "Examples:\n  {examples}\n\n\
             Answers `EROFS is read-only` (exit 3) before opening the image or reading \
             stdin. Rebuild the image with mkfs.erofs instead."
        ))
}

fn mutation_path() -> Arg {
    Arg::new("path")
        .value_name("PATH")
        .required(true)
        .value_parser(value_parser!(OsString))
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
        "ls" if sub.get_flag("recursive") => {
            ls_recursive(&open(target, offset)?, path_arg(sub), sub)
        }
        "ls" => ls(&open(target, offset)?, path_arg(sub)),
        "stat" => stat(&open(target, offset)?, path_arg(sub)),
        "read" => read(&open(target, offset)?, path_arg(sub), sub.get_one("output")),
        "export" => {
            let request = transfer::ExportRequest::from_matches(sub)?;
            let tree = transfer::TreeOptions::from_matches(sub)?;
            let fs = open(target, offset)?;
            if tree.recursive {
                fs.set_pcluster_cache_capacity(0);
                transfer::export_tree_with_times(
                    &request,
                    &mut TreeExport {
                        fs: &fs,
                        image: std::path::Path::new(target),
                    },
                    tree,
                    transfer::LinkOptions::from_matches(sub)?,
                    transfer::TimeOptions::from_matches(sub)?,
                )
            } else {
                export(&fs, &request, transfer::ExportOptions::from_matches(sub)?)
            }
        }
        "xattr" => xattr(&open(target, offset)?, path_arg(sub), sub),
        "get" | "info" => get(
            &open(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        // Refused before the image or import source is opened, or stdin is
        // read: nothing about the target can change the answer. Core 0.3.14
        // has no shared import API, so import uses the existing refusal.
        "import" | "write" | "mkdir" | "rm" | "rmdir" | "mv" | "ln" | "truncate" | "set"
        | "resize" => Err(CliError::refused(format!(
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
        e.get("path")
            .map(Json::to_text)
            .unwrap_or_else(|| field("name"))
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    if e.get("revisited").is_some() {
        line.push_str(" [revisited]");
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

fn recursive_entry(
    fs: &Filesystem,
    path: &[u8],
    depth: u64,
    inode: &Inode,
    revisited: bool,
) -> Result<Json, CliError> {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    let Json::Obj(mut fields) = entry(fs, name, inode).map_err(|e| erofs_error(path, e))? else {
        unreachable!()
    };
    fields.push(("path".into(), Json::from(show(path))));
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex".into(),
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.push(("depth".into(), Json::from(depth)));
    if revisited {
        fields.push(("revisited".into(), Json::from(true)));
    }
    Ok(Json::Obj(fields))
}

/// Directory iterators live on an explicit stack, so a large requested
/// depth cannot overflow the process call stack. Bounds exclude the next
/// entry; empty directories and exact fits still succeed.
fn ls_recursive(fs: &Filesystem, path: &[u8], sub: &ArgMatches) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| erofs_error(path, e))?;
    let max_depth = sub.get_one::<u64>("max-depth").copied().unwrap_or(64);
    let limit = sub.get_one::<u64>("limit").copied().unwrap_or(100_000);
    let mut full = Vec::new();
    for component in path.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        full.push(b'/');
        full.extend_from_slice(component);
    }
    if full.is_empty() {
        full.push(b'/');
    }
    let mut entries = Vec::new();
    let result = (|| {
        if !inode.is_dir() {
            if limit == 0 {
                return Err(CliError::failed(format!(
                    "--limit 0 reached at {}",
                    show(&full)
                )));
            }
            entries.push(recursive_entry(fs, &full, 0, &inode, false)?);
            return Ok(());
        }
        let children = |node: &Inode, at: &[u8]| {
            let mut children = fs.read_dir(node).map_err(|e| erofs_error(at, e))?;
            children.retain(|e| e.name != b"." && e.name != b"..");
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok::<_, CliError>(children.into_iter())
        };
        let mut visited = HashSet::from([inode.nid]);
        let first = children(&inode, &full)?;
        let mut stack = vec![(full, 0u64, first)];
        while let Some((parent, parent_depth, children_iter)) = stack.last_mut() {
            let Some(child) = children_iter.next() else {
                stack.pop();
                continue;
            };
            let mut full = parent.clone();
            if !full.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(&child.name);
            let depth = *parent_depth + 1;
            if entries.len() as u64 >= limit {
                return Err(CliError::failed(format!(
                    "--limit {limit} reached at {}",
                    show(&full)
                )));
            }
            if depth > max_depth {
                return Err(CliError::failed(format!(
                    "--max-depth {max_depth} reached at {}",
                    show(&full)
                )));
            }
            let inode = fs
                .read_inode(child.nid)
                .map_err(|e| erofs_error(&full, e))?;
            let revisited = inode.is_dir() && !visited.insert(inode.nid);
            entries.push(recursive_entry(fs, &full, depth, &inode, revisited)?);
            if inode.is_dir() && !revisited {
                let next = children(&inode, &full)?;
                stack.push((full, depth, next));
            }
        }
        Ok(())
    })();
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    let report = Outcome::report(Json::Arr(entries)).with_text(text);
    match result {
        Ok(()) => Ok(report),
        Err(error) => {
            // Outcome carries one stream. Render the partial report here
            // and let the dispatcher render the error in the same format.
            let format = fs_core::cli::Format::of(sub, &std::env::args_os().collect::<Vec<_>>());
            let partial = fs_core::cli::output::render("fs.erofs", format, Ok(report));
            std::io::stdout()
                .lock()
                .write_all(partial.stdout.as_bytes())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            Err(error)
        }
    }
}

/// Report the inode itself, retaining absent fields as null rather than
/// copying mtime into access/change time as a mounted kernel does.
fn stat(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| erofs_error(path, e))?;
    let sb = fs.superblock();
    let (layout, seconds, nanos) = match inode.format.version {
        InodeVersion::Compact => ("compact", sb.build_time, sb.build_time_nsec),
        InodeVersion::Extended => ("extended", inode.mtime, inode.mtime_nsec),
    };
    // EROFS accounts compressed data with i_u's compressed-block count.
    // Other data layouts charge the rounded logical size, even inline
    // tails and sparse chunks; device IDs are never block counts.
    let blocks = match inode.file_type() {
        FileType::RegularFile | FileType::Dir | FileType::Symlink => match inode.format.layout {
            DataLayout::Compression | DataLayout::CompressionLegacy => u64::from(inode.raw_u),
            _ => inode.size.div_ceil(sb.block_size()),
        },
        _ => 0,
    };
    let allocated = blocks
        .checked_mul(sb.block_size())
        .ok_or_else(|| CliError::failed(format!("{}: allocated size overflows", show(path))))?;
    let datalayout = match inode.format.layout {
        DataLayout::FlatPlain => "flat_plain",
        DataLayout::FlatInline => "flat_inline",
        DataLayout::CompressionLegacy => "compression_legacy",
        DataLayout::Compression => "compression",
        DataLayout::ChunkBased => "chunk_based",
    };
    let mut fields = vec![
        ("path", Json::from(show(path))),
        ("inode", Json::from(inode.nid)),
        ("type", Json::from(type_name(inode.file_type()))),
        ("mode", Json::from(format!("{:04o}", inode.mode & 0o7777))),
        ("uid", Json::from(inode.uid)),
        ("gid", Json::from(inode.gid)),
        ("nlink", Json::from(inode.nlink)),
        ("size", Json::from(inode.size)),
        ("allocated_bytes", Json::from(allocated)),
        ("atime", Json::Null),
        ("atime_nsec", Json::Null),
        ("mtime", Json::from(seconds as i64)),
        ("mtime_nsec", Json::from(nanos)),
        ("ctime", Json::Null),
        ("ctime_nsec", Json::Null),
        ("btime", Json::Null),
        ("btime_nsec", Json::Null),
    ];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    if inode.is_symlink() {
        let target = fs
            .read_symlink_target(&inode)
            .map_err(|e| erofs_error(path, e))?;
        fields.push(("target", Json::from(show(&target))));
    }
    if let Some((major, minor)) = inode.rdev() {
        fields.extend([("major", Json::from(major)), ("minor", Json::from(minor))]);
    }
    let xattrs = fs.xattrs(&inode).map_err(|e| erofs_error(path, e))?;
    fields.push((
        "erofs",
        Json::object([
            ("layout", Json::from(layout)),
            ("datalayout", Json::from(datalayout)),
            ("xattr_count", Json::from(xattrs.len() as u64)),
            (
                "flags",
                Json::from(format!("0x{:04x}", inode.format.flags << 4)),
            ),
        ]),
    ));
    Ok(Outcome::report(Json::object(fields)))
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

/// EROFS provides exact positioned reads; core owns copying and publication.
struct ExportSource<'a> {
    fs: &'a Filesystem,
    inode: Inode,
    path: &'a [u8],
}

impl transfer::FileSource for ExportSource<'_> {
    fn size(&self) -> u64 {
        self.inode.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, CliError> {
        self.fs
            .read_file(&self.inode, offset, buf)
            .map_err(|e| erofs_error(self.path, e))?;
        Ok(buf.len())
    }
}

fn export(
    fs: &Filesystem,
    request: &transfer::ExportRequest,
    options: transfer::ExportOptions,
) -> Result<Outcome, CliError> {
    let path = os_bytes(&request.path);
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| erofs_error(path, e))?;
    if inode.file_type() != FileType::RegularFile {
        return Err(CliError::failed(format!(
            "{}: not a regular file; directories, symlinks and special files cannot be exported",
            show(path)
        )));
    }
    // Export reads once in order: retaining decompressed clusters would
    // keep earlier chunks alive after core has moved past them.
    fs.set_pcluster_cache_capacity(0);
    transfer::export_with_options(request, &mut ExportSource { fs, inode, path }, options)
}

/// EROFS only supplies filesystem operations. Core owns traversal, depth,
/// safe names, destination merging, publication and per-item reports.
struct TreeExport<'a> {
    fs: &'a Filesystem,
    image: &'a std::path::Path,
}

impl TreeExport<'_> {
    fn resolve(&self, path: &std::path::Path) -> Result<(Vec<u8>, Inode), CliError> {
        let bytes = os_bytes(&path.as_os_str().to_owned()).to_vec();
        let inode = self
            .fs
            .lookup_path_bytes(&bytes)
            .map_err(|e| erofs_error(&bytes, e))?;
        Ok((bytes, inode))
    }

    fn metadata_for(&self, inode: &Inode) -> transfer::TreeMetadata {
        let (seconds, nanos) = match inode.format.version {
            InodeVersion::Compact => (
                self.fs.superblock().build_time,
                self.fs.superblock().build_time_nsec,
            ),
            InodeVersion::Extended => (inode.mtime, inode.mtime_nsec),
        };
        transfer::TreeMetadata {
            kind: match inode.file_type() {
                FileType::RegularFile => transfer::TreeKind::File,
                FileType::Dir => transfer::TreeKind::Directory,
                _ => transfer::TreeKind::Other,
            },
            size: inode.size,
            inode: inode.nid,
            // EROFS does not store access times; do not invent one from mtime.
            accessed: None,
            modified: std::time::Duration::from_secs(seconds)
                .checked_add(std::time::Duration::from_nanos(u64::from(nanos)))
                .and_then(|time| std::time::UNIX_EPOCH.checked_add(time)),
        }
    }
}

#[cfg(unix)]
fn os_name(name: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(name)
}

#[cfg(not(unix))]
fn os_name(name: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&name).into_owned())
}

impl transfer::TreeSource for TreeExport<'_> {
    fn symbolic_link(
        &mut self,
        path: &std::path::Path,
    ) -> Result<Option<transfer::SymbolicLink>, CliError> {
        let (bytes, inode) = self.resolve(path)?;
        if !inode.is_symlink() {
            return Ok(None);
        }
        let target = self
            .fs
            .read_symlink_target(&inode)
            .map_err(|e| erofs_error(&bytes, e))?;
        Ok(Some(transfer::SymbolicLink {
            target: os_name(target).into(),
            // EROFS stores no directory bit for symlinks. Never resolve
            // the target to guess one, even when it happens to exist.
            target_is_directory: false,
        }))
    }

    fn image_path(&self) -> Option<&std::path::Path> {
        Some(self.image)
    }

    fn metadata(&mut self, path: &std::path::Path) -> Result<transfer::TreeMetadata, CliError> {
        let (_, inode) = self.resolve(path)?;
        Ok(self.metadata_for(&inode))
    }

    fn read_dir(&mut self, path: &std::path::Path) -> Result<Vec<transfer::TreeEntry>, CliError> {
        let (bytes, inode) = self.resolve(path)?;
        self.fs
            .read_dir(&inode)
            .map_err(|e| erofs_error(&bytes, e))?
            .into_iter()
            .filter(|entry| entry.name != b"." && entry.name != b"..")
            .map(|entry| {
                let child = self.fs.read_inode(entry.nid).map_err(|e| {
                    let mut full = bytes.clone();
                    if !full.ends_with(b"/") {
                        full.push(b'/');
                    }
                    full.extend_from_slice(&entry.name);
                    erofs_error(&full, e)
                })?;
                Ok(transfer::TreeEntry {
                    name: os_name(entry.name),
                    metadata: self.metadata_for(&child),
                })
            })
            .collect()
    }

    fn read_file_at(
        &mut self,
        path: &std::path::Path,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, CliError> {
        let (bytes, inode) = self.resolve(path)?;
        if inode.file_type() != FileType::RegularFile {
            return Err(CliError::failed(format!(
                "{}: not a regular file",
                show(&bytes)
            )));
        }
        self.fs
            .read_file(&inode, offset, buf)
            .map_err(|e| erofs_error(&bytes, e))?;
        Ok(buf.len())
    }
}

fn xattr(fs: &Filesystem, path: &[u8], sub: &ArgMatches) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| erofs_error(path, e))?;
    let mut attrs = fs.xattrs(&inode).map_err(|e| erofs_error(path, e))?;
    if let Some(name) = sub.get_one::<OsString>("name") {
        let (_, value) = attrs
            .into_iter()
            .find(|(attr, _)| attr.as_slice() == os_bytes(name))
            .ok_or_else(|| {
                CliError::failed(format!(
                    "{}: no attribute {}",
                    show(path),
                    show(os_bytes(name))
                ))
            })?;
        match sub.get_one::<OsString>("output") {
            None => {
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(&value)
                    .and_then(|()| stdout.flush())
                    .map_err(|e| CliError::failed(format!("write: {e}")))?;
            }
            Some(file) => {
                let dest = std::path::Path::new(file);
                let mut partial = file.clone();
                partial.push(".partial");
                let result =
                    std::fs::write(&partial, &value).and_then(|()| std::fs::rename(&partial, dest));
                if let Err(e) = result {
                    let _ = std::fs::remove_file(&partial);
                    return Err(CliError::failed(format!("write {}: {e}", dest.display())));
                }
            }
        }
        return Ok(Outcome::done());
    }
    attrs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut entries = Vec::new();
    let mut text = Vec::new();
    for (name, value) in attrs {
        let mut fields = vec![("name", Json::from(show(&name)))];
        if std::str::from_utf8(&name).is_err() {
            fields.push((
                "name_hex",
                Json::from(name.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            ));
        }
        fields.push(("size", Json::from(value.len() as u64)));
        entries.push(Json::object(fields));
        text.push(format!("{}\t{}", value.len(), show(&name)));
    }
    Ok(Outcome::report(Json::Arr(entries)).with_text(text.join("\n")))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `kind` the JSON error carries, as compact JSON.
    fn json_kind(e: &CliError) -> Option<String> {
        e.to_json().get("kind").map(Json::to_compact)
    }

    #[test]
    fn a_missing_path_is_the_not_found_kind() {
        // What lookup_path_bytes answers for a name that is not there, or
        // for one under a parent that is not there.
        let e = erofs_error(b"/missing/f", fs_erofs::Error::NotFound);
        let json = e.to_json().to_compact();
        assert_eq!(e.code, fs_core::cli::output::EXIT_FAILED, "{json}");
        assert_eq!(json_kind(&e).as_deref(), Some("\"not_found\""), "{json}");
        assert!(e.message.starts_with("/missing/f: "), "{json}");
    }

    #[test]
    fn damage_and_a_file_on_the_path_carry_no_kind() {
        for e in [
            fs_erofs::Error::NotADirectory,
            fs_erofs::Error::BadDirent("name past the block"),
            fs_erofs::Error::NotErofs,
        ] {
            let e = erofs_error(b"/f", e);
            assert_eq!(json_kind(&e), None, "{}", e.to_json().to_compact());
        }
    }
}
