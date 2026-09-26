//! The test contract, checked: THE ORACLE TOOLS RUN IN THE HARNESS VM
//! AND NOWHERE ELSE, the kernel is only ever asked in the guest, and no
//! test announces a skip.
//!
//! Why the host is forbidden rather than merely second choice:
//! erofs-utils on a workstation is whatever that machine has — Debian 12
//! packages 1.5, Ubuntu 24.04 packages 1.7.1, neither knows the options
//! the writer tests use, most packaged builds cannot write ZSTD at all,
//! and on a Mac there is no EROFS. One version, in one guest, answers
//! the same way for everyone. So a test that spawns `fsck.erofs` itself
//! is refused here even when it would work on the machine that wrote it.
//!
//! `chore test:unit`, `chore test:oracle` and `chore test:kernel` are
//! chosen by `scripts/test-targets.sh` from what each test file calls:
//! `fs_erofs_test_support::oracle` / `assert_fsck_clean` /
//! `mkfs_from_guest_tree` for a tool, the kernel helpers for a mount, or
//! a path under the fixture directory for an image. That is only sound while those are the only
//! ways in: a test that spawned a tool by name would be classified as a
//! unit test, run on the `unit` CI job with no VM, and — worse — be free
//! to return early when the tool is absent, which is the silent pass the
//! contract exists to end. So this file reads every test source and
//! refuses every other shape.
//!
//! It names the patterns it looks for without spelling them out, so that
//! this file itself stays in the unit tier.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `dir`, recursively, except the support crate
/// (which is where the sanctioned helpers live) and this file (whose
/// self-test spells out the shapes it refuses).
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "support") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with(file!()) {
            out.push(path);
        }
    }
}

fn all_test_sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_sources(&manifest_dir().join("tests"), &mut files);
    rust_sources(&manifest_dir().join("src"), &mut files);
    assert!(
        files.len() > 30,
        "found only {} sources; the scan is looking in the wrong place",
        files.len()
    );
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            (p, text)
        })
        .collect()
}

/// The erofs-utils programs the oracle tests use, and the two staging
/// tools whose answers depend on the filesystem underneath them —
/// setting an xattr on the host and reading it in the guest is a
/// question about 9p, not about EROFS, so both belong in the guest too.
const TOOLS: [&str; 6] = [
    "mkfs.erofs",
    "fsck.erofs",
    "dump.erofs",
    "erofsfuse",
    "setfattr",
    "getfattr",
];

/// Places in `text` where a process is spawned from a string literal that
/// names an oracle tool, or from a hard-coded sbin path.
fn direct_tool_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let program = &literal[..end];
        let named = TOOLS.contains(&program)
            || program.contains("sbin/")
            || TOOLS.iter().any(|t| program.ends_with(&format!("/{t}")));
        if named {
            hits.push(program.to_string());
        }
    }
    // A probe of a fixed install path is how the old "is fsck.erofs
    // here?" skips found their tool.
    for line in text.lines() {
        let probe = ["\"/usr/", "sbin/"].concat();
        let probe_root = ["\"/", "sbin/"].concat();
        if !line.contains(&spawn)
            && (line.contains(&probe) || line.contains(&probe_root))
            && TOOLS.iter().any(|t| line.contains(t))
        {
            hits.push(line.trim().to_string());
        }
    }
    hits
}

/// Places in `text` that spawn a program NAMED BY A VARIABLE.
///
/// The scans above read the literal a process is spawned with, so a test
/// that puts the tool's name in a variable first would walk past them —
/// and that is not a hypothetical: every oracle test used to do exactly
/// that, with a `run(program, args)` helper. The only programs a test
/// spawns by computed name are this crate's own binaries, which come
/// from `CARGO_BIN_EXE_*`, so the rule is: a non-literal program must be
/// one of those, in the same file.
fn indirect_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let own_binary = ["CARGO_BIN", "_EXE"].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        if rest.starts_with('"') {
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            hits.push(rest.lines().next().unwrap_or_default().trim().to_string());
            continue;
        }
        // The binding it came from, wherever it is in the file.
        let bound_to_own_binary = text.lines().any(|line| {
            (line.contains(&format!("let {name} ="))
                || line.contains(&format!("let {name}:"))
                || line.contains(&format!("const {name}:")))
                && line.contains(&own_binary)
        });
        if !bound_to_own_binary {
            hits.push(name);
        }
    }
    hits
}

/// Programs that reach the VM or mount a filesystem. A test drives
/// neither itself: the harness is spoken to in one place (the support
/// crate), so there is one answer to "is the VM up", one place that
/// boots it, and no test that mounts anything on the machine running it.
const HARNESS: [&str; 6] = ["vagrant", "ssh", "mount", "umount", "losetup", "vm.sh"];

/// Places in `text` that spawn one of those.
fn harness_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let program = &literal[..end];
        let last = program.rsplit('/').next().unwrap_or(program);
        if HARNESS.contains(&last) {
            hits.push(program.to_string());
        }
    }
    hits
}

/// Lines that print a skip notice: the signature of a test that returns
/// early and passes having checked nothing.
fn announced_skips(text: &str) -> Vec<String> {
    let print = ["eprint", "ln!("].concat();
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(&print) {
            continue;
        }
        // The message may sit on the next line or two after rustfmt.
        let window = lines[i..lines.len().min(i + 3)].join(" ").to_lowercase();
        if window.contains("skip") {
            hits.push(format!("line {}: {}", i + 1, line.trim()));
        }
    }
    hits
}

#[test]
fn no_test_runs_an_oracle_tool_on_the_host() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in direct_tool_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these run an oracle tool on the HOST. The tools live in the harness VM and \
         nowhere else: use fs_erofs_test_support::oracle, which runs them there and \
         fails, naming the task that fixes it, when it cannot:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_announces_a_skip() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        // ONLY WHERE THERE IS A TEST TO SKIP. `src/bin/mkfs_erofs.rs`
        // warns on stderr that it is skipping a non-UTF-8 directory
        // entry, and that is the PROGRAM telling its user what it left
        // out — a message tests/cli.rs asserts on. A file with no
        // `#[test]` in it cannot announce a test's skip, so the rule is
        // scoped to files that have one rather than to a path, which
        // would also exempt any test somebody put there later.
        if !text.contains(&["#[", "test]"].concat()) {
            continue;
        }
        for hit in announced_skips(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these print a skip notice. A test never skips on a missing tool or \
         fixture; fail instead (fixture / oracle do):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_drives_the_vm_or_mounts_a_filesystem_itself() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in harness_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these drive the VM or mount a filesystem themselves. The guest is reached \
         through fs_erofs_test_support (the oracle and kernel helpers), which boots it once \
         per process and keeps one connection; a mount happens only inside the \
         guest, never on the host:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_spawns_a_program_it_named_in_a_variable() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in indirect_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these spawn a program whose name is in a variable, which the checks above \
         cannot read. Only this crate's own binaries (CARGO_BIN_EXE_*) are spawned \
         that way; an oracle tool goes through fs_erofs_test_support::oracle:\n{}",
        offenders.join("\n")
    );
}

/// The scans find what they are for, so the two tests above cannot pass
/// by looking at nothing.
#[test]
fn the_scans_recognise_the_shapes_they_refuse() {
    let spawn = [
        "let out = Command",
        "::new(\"fsck.erofs\").arg(img).output();\n",
        "let dbg = Command",
        "::new(\"/usr/sbin/dump.erofs\");\n",
        "let ok = Command",
        "::new(tool).arg(img);\n",
        "let found = [\"/usr/",
        "sbin/fsck.erofs\", \"/",
        "sbin/fsck.erofs\"].into_iter().find(|p| exists(p));\n",
    ]
    .concat();
    let hits = direct_tool_spawns(&spawn);
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert_eq!(
        hits[..2],
        ["fsck.erofs".to_string(), "/usr/sbin/dump.erofs".to_string()]
    );

    let indirect = [
        "const MKFS: &str = env!(\"CARGO_BIN",
        "_EXE_mkfs_erofs\");\n",
        "let out = Command",
        "::new(MKFS).output();\n",
        "let out = Command",
        "::new(tool).args(args).output();\n",
        "Command",
        "::new(\"sh\").arg(\"-c\");\n",
    ]
    .concat();
    assert_eq!(indirect_spawns(&indirect), ["tool".to_string()]);

    let harness = [
        "let vm = Command",
        "::new(\"../fs-linux-test-harness/scripts/vm.sh\");\n",
        "Command",
        "::new(\"mount\").args([\"-t\", \"erofs\"]);\n",
        "Command",
        "::new(\"cargo\").arg(\"test\");\n",
    ]
    .concat();
    assert_eq!(
        harness_spawns(&harness),
        [
            "../fs-linux-test-harness/scripts/vm.sh".to_string(),
            "mount".to_string()
        ]
    );

    let skip = [
        "if missing {\n    eprint",
        "ln!(\n        \"SKIP: no image\"\n    );\n    return;\n}\n",
        "eprint",
        "ln!(\"note: took {ms} ms\");\n",
    ]
    .concat();
    assert_eq!(announced_skips(&skip).len(), 1);
}
