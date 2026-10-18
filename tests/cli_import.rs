//! Import is a format refusal before any input or image is opened.
//! OS file offsets and blocking FIFOs expose accidental I/O independently
//! of the filesystem parser. These tests need no fixture or external tool.

mod cli_support;
mod common;

use cli_support::*;
use std::fs::File;
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const REFUSAL: &str = "{\"error\": \"import: EROFS is read-only; build a new image with mkfs.erofs instead\", \"code\": 3}\n";

fn image(root: &Path) -> (PathBuf, Vec<u8>) {
    let bytes = fs_erofs::mkfs::build_image(
        common::dir(vec![("existing", common::file(b"keep these bytes"))]),
        12,
    )
    .unwrap();
    let target = root.join("target.erofs");
    std::fs::write(&target, &bytes).unwrap();
    (target, bytes)
}

fn import(target: &Path, source: &Path) -> Command {
    let mut cmd = tool("fs.erofs");
    cmd.arg(target)
        .arg("import")
        .arg(source)
        .arg("/new")
        .stdin(Stdio::null());
    cmd
}

// A regression that opens a FIFO must fail, rather than hang the suite.
fn bounded(mut cmd: Command) -> Output {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn import");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!("import blocked on input or image: {}", stderr(&out));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[track_caller]
fn refused(out: Output) {
    refused_as(out, false);
}

#[track_caller]
fn refused_as(out: Output, text: bool) {
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
    let expected = if text {
        "fs.erofs: import: EROFS is read-only; build a new image with mkfs.erofs instead\n"
    } else {
        REFUSAL
    };
    assert_eq!(stderr(&out), expected);
}

#[test]
fn stdin_offset_and_every_image_byte_survive_import_refusal() {
    let root = scratch("import-stdin");
    let (target, before) = image(&root);
    let source = root.join("stdin");
    std::fs::write(&source, pattern(2 << 20, 88)).unwrap();
    let mut input = File::open(&source).unwrap();
    for flags in [&[][..], &["--replace"][..], &["--text"][..]] {
        let mut cmd = import(&target, Path::new("-"));
        cmd.args(flags).stdin(input.try_clone().unwrap());
        refused_as(bounded(cmd), flags.contains(&"--text"));
        assert_eq!(input.stream_position().unwrap(), 0, "stdin was consumed");
        assert_eq!(std::fs::read(&target).unwrap(), before);
    }
}

#[test]
fn refusal_precedes_source_and_image_validation() {
    let root = scratch("import-paths");
    let (target, before) = image(&root);
    let source = root.join("source");
    std::fs::write(&source, b"source bytes").unwrap();
    let absent = root.join("absent");
    for target in [&target, &source, &absent] {
        for source in [&source, &root, &absent] {
            let mut cmd = import(target, source);
            cmd.args(["--replace", "--offset", "18446744073709551615"]);
            refused(bounded(cmd));
        }
    }
    assert_eq!(std::fs::read(&target).unwrap(), before);
    assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
    assert!(!absent.exists());
}

#[cfg(unix)]
fn fifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    // mode_t differs between the two supported Unix platforms.
    #[cfg(target_os = "macos")]
    type Mode = u16;
    #[cfg(not(target_os = "macos"))]
    type Mode = u32;
    extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: Mode) -> std::ffi::c_int;
    }
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL-terminated and remains valid throughout the call.
    assert_eq!(unsafe { mkfifo(name.as_ptr(), 0o600) }, 0);
}

#[test]
#[cfg(unix)]
fn a_source_fifo_without_a_writer_is_never_opened() {
    let root = scratch("import-source-fifo");
    let (target, before) = image(&root);
    let source = root.join("source.fifo");
    fifo(&source);
    refused(bounded(import(&target, &source)));
    assert_eq!(std::fs::read(&target).unwrap(), before);
}

#[test]
#[cfg(unix)]
fn an_image_fifo_without_a_writer_is_never_opened() {
    let root = scratch("import-image-fifo");
    let target = root.join("target.fifo");
    fifo(&target);
    refused(bounded(import(&target, &root.join("missing-source"))));
}

#[test]
fn missing_import_operands_remain_usage_errors() {
    for args in [&["import"][..], &["import", "-"][..]] {
        let out = tool("fs.erofs")
            .arg("missing.erofs")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        assert!(out.stdout.is_empty());
    }
}
