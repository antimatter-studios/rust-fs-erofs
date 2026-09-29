//! The multi-call binary: every name answers as itself, the
//! repository-named form reaches the same tool, `--version` identifies
//! the crate, errors are structured, and `doctor` tells our program from
//! whatever else PATH finds under the same name.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use std::path::Path;

const CRATE: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn every_name_answers_version_with_itself_the_crate_and_the_version() {
    let mut names = dotted_names();
    assert!(names.contains(&"mkfs.erofs".to_string()), "{names:?}");
    names.push("rust-fs-erofs".to_string());
    for name in names {
        for flag in ["--version", "-V"] {
            let out = ok(tool(&name).arg(flag));
            assert_eq!(
                stdout(&out).trim_end(),
                format!("{name} ({CRATE}) {VERSION}"),
                "{name} {flag}"
            );
        }
    }
}

#[test]
fn the_repository_name_reaches_a_tool_by_verb_and_by_full_name() {
    let src = scratch("repo-form-src");
    write_tree(&src, &[("a", pattern(100, 1)), ("d/b", pattern(5000, 2))]);
    let src = src.display().to_string();
    let img = image_path("repo-form");
    let dotted = ok(tool("mkfs.erofs").args(["-q", &img, &src]));
    let first = std::fs::read(&img).unwrap();
    for word in ["mkfs", "mkfs.erofs"] {
        let repo = ok(tool("rust-fs-erofs").args([word, "-q", &img, &src]));
        assert_eq!(stdout(&repo), stdout(&dotted), "rust-fs-erofs {word}");
        assert_eq!(std::fs::read(&img).unwrap(), first, "rust-fs-erofs {word}");
    }
    // cargo's own build, under cargo's name, is the same entry point.
    let cargo = ok(entry().args(["mkfs", "-q", &img, &src]));
    assert_eq!(stdout(&cargo), stdout(&dotted));
}

#[test]
fn every_tool_help_carries_an_example() {
    for name in dotted_names() {
        let out = ok(tool(&name).arg("--help"));
        assert!(
            stdout(&out).contains("Examples:"),
            "{name} --help has no example:\n{}",
            stdout(&out)
        );
    }
    let out = ok(tool("rust-fs-erofs").arg("--help"));
    for name in dotted_names() {
        let verb = name.split('.').next().unwrap();
        assert!(
            stdout(&out).contains(&format!("rust-fs-erofs {verb}")),
            "rust-fs-erofs --help does not show `rust-fs-erofs {verb}`:\n{}",
            stdout(&out)
        );
    }
}

#[test]
fn a_wrong_command_line_is_a_structured_error_on_stderr_with_status_2() {
    let out = tool("mkfs.erofs")
        .args(["--no-such-flag", "x.img"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with("{\"error\": \"") && err.trim_end().ends_with("\"code\": 2}"),
        "{err}"
    );
    assert!(err.contains("--no-such-flag"), "{err}");

    // --text: clap's own message, for a person.
    let out = tool("mkfs.erofs")
        .args(["--text", "--no-such-flag", "x.img"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).starts_with("error: "), "{}", stderr(&out));
}

#[test]
fn a_failed_run_is_a_structured_error_on_stderr_with_status_1() {
    let missing = image_path("never-created");
    let img = image_path("never-written");
    let out = tool("mkfs.erofs").args([&img, &missing]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    let last = stderr(&out).lines().last().unwrap_or("").to_string();
    assert!(
        last.starts_with("{\"error\": \"walking ") && last.ends_with("\"code\": 1}"),
        "{last}"
    );
    assert!(!Path::new(&img).exists(), "a failed run left {img} behind");
    let out = tool("mkfs.erofs")
        .args(["--text", &img, &missing])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("mkfs.erofs: walking "),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// A PATH made of `dirs`, and doctor's JSON and status against it.
fn doctor(dirs: &[&Path]) -> (Option<i32>, String) {
    let path = std::env::join_paths(dirs).unwrap();
    let out = entry().arg("doctor").env("PATH", path).output().unwrap();
    (out.status.code(), stdout(&out))
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(fs_erofs_test_support::temp_path!(
        "cli-doctor-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An executable script at `path` that prints `line` for `--version`.
fn impostor(path: &Path, line: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn doctor_passes_when_every_name_on_path_is_ours() {
    let (code, json) = doctor(&[names_dir()]);
    assert_eq!(code, Some(0), "{json}");
    assert!(json.contains("\"ok\": true"), "{json}");
    for name in dotted_names() {
        assert!(json.contains(&format!("\"name\": \"{name}\"")), "{json}");
    }
    assert!(!json.contains("\"status\": \"missing\""), "{json}");
}

#[test]
fn doctor_names_a_shadowing_program_and_says_which_path_entry_to_move() {
    let theirs = scratch_dir("foreign");
    // erofs-utils' own banner: our shape, another package's name.
    impostor(&theirs.join("mkfs.erofs"), "mkfs.erofs (erofs-utils) 1.9.1");
    let (code, json) = doctor(&[&theirs, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"ok\": false"), "{json}");
    assert!(json.contains("\"status\": \"foreign\""), "{json}");
    assert!(
        json.contains(&format!(
            "\"path\": \"{}\"",
            theirs.join("mkfs.erofs").display()
        )),
        "{json}"
    );
    assert!(
        json.contains(&format!(
            "put {} before {} on PATH",
            names_dir().display(),
            theirs.display()
        )),
        "{json}"
    );
    // Ours is still found, later, and listed as not run.
    assert!(
        json.contains(&names_dir().join("mkfs.erofs").display().to_string()),
        "{json}"
    );
}

#[test]
fn doctor_names_the_homebrew_formula_to_unlink() {
    let prefix = scratch_dir("brew");
    let real = prefix.join("Cellar/erofs-utils/1.9.1/bin/mkfs.erofs");
    impostor(&real, "mkfs.erofs (erofs-utils) 1.9.1");
    let bin_dir = prefix.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::os::unix::fs::symlink(&real, bin_dir.join("mkfs.erofs")).unwrap();
    let (code, json) = doctor(&[&bin_dir, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"formula\": \"erofs-utils\""), "{json}");
    assert!(json.contains("`brew unlink erofs-utils`"), "{json}");
}

#[test]
fn doctor_reports_a_missing_name_with_how_to_install_it() {
    let empty = scratch_dir("empty");
    let (code, json) = doctor(&[&empty]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"status\": \"missing\""), "{json}");
    assert!(json.contains("chore cli:install"), "{json}");
    assert!(
        json.contains("brew install antimatter-studios/tap/rust-fs-erofs"),
        "{json}"
    );
}

#[test]
fn doctor_reports_our_program_at_another_version_as_stale() {
    let old = scratch_dir("stale");
    impostor(
        &old.join("mkfs.erofs"),
        &format!("mkfs.erofs ({CRATE}) 0.0.1"),
    );
    let (code, json) = doctor(&[&old, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"status\": \"stale\""), "{json}");
    assert!(
        json.contains(&format!("{CRATE} 0.0.1, not {VERSION}")),
        "{json}"
    );
}

#[test]
fn doctor_text_is_for_a_person_and_keeps_the_fix() {
    let theirs = scratch_dir("text");
    impostor(&theirs.join("mkfs.erofs"), "something else entirely");
    let path = std::env::join_paths([theirs.as_path(), names_dir()]).unwrap();
    let out = entry()
        .args(["doctor", "--text"])
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("mkfs.erofs: foreign ("), "{text}");
    assert!(text.contains("  fix: "), "{text}");
    assert!(!text.contains('{'), "{text}");
}
