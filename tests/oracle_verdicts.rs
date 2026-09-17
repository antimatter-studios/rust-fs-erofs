//! An oracle test has to reach a verdict, and a failed read is not one.
//!
//! `tests/oracle_compat.rs` builds images with the real `mkfs.erofs` and
//! reads them back with this crate. Six of those tests used to end in a
//! match whose middle arm listed every failure variant of the outcome
//! enum and did nothing with them: only a WRONG answer failed, while an
//! image the reader could not open at all, could not look a path up in,
//! or errored on reading, passed. The class of regression those tests
//! are best placed to catch -- a misparsed superblock field, a rejected
//! zmap header, a decompressor returning an error for every cluster --
//! was precisely the class they were configured to ignore (#116).
//!
//! The six are fixed in place. This file is what stops the shape being
//! written again in the next oracle test, the way `tests/ci_profile.rs`
//! stops the workflow's debug run being removed again. It reads the
//! test sources as text and refuses an arm that names two or more of
//! the failure variants and then does nothing.
//!
//! # Not fooled by its own source
//!
//! A guard spelled with the exact text it forbids has to exempt its own
//! file, and that exemption is a hole the size of the guard: a test
//! could be moved into the exempt file and go unseen. So this file
//! never spells the variant names -- it assembles them at run time --
//! and it scans EVERY `.rs` file under `tests/`, including itself. The
//! assertions below prove both: that the scan reached `oracle_compat.rs`
//! (the file it exists for) and that it reached this one.

use std::path::{Path, PathBuf};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// The failure variants of `oracle_compat.rs`'s `ReadOutcome`, built by
/// concatenation so no part of this file contains one of them as text.
fn failure_variants() -> [String; 3] {
    ["Open", "Lookup", "Read"].map(|step| [step, "Error"].concat())
}

/// Everything in `line` that the compiler sees: a `//` comment is cut,
/// but only when it really starts a comment and is not inside a string
/// literal, so a scan cannot be blinded by a URL in a message.
fn without_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// The source with comments removed and every run of whitespace reduced
/// to one space, so an arm written across four lines reads as one, and
/// an empty body reads the same whether it was written `{}` or `{ }`.
fn code_only(source: &str) -> String {
    let joined: Vec<&str> = source.lines().map(without_comment).collect();
    joined
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("{ }", "{}")
        .replace("( )", "()")
}

/// Match arms that name two or more failure variants and do nothing.
///
/// The arm is taken to start after the nearest preceding `,`, `;`, `{`
/// or `}` -- the separators that end the previous arm -- so a neighbour
/// arm's text cannot be counted towards this one's.
fn arms_that_swallow_a_failure(source: &str) -> Vec<String> {
    let code = code_only(source);
    let variants = failure_variants();
    let mut found = Vec::new();
    for empty_body in ["=> {}", "=> ()"] {
        let mut from = 0;
        while let Some(offset) = code[from..].find(empty_body) {
            let at = from + offset;
            let start = code[..at].rfind([',', ';', '{', '}']).map_or(0, |p| p + 1);
            let arm = code[start..at + empty_body.len()].trim();
            let named = variants.iter().filter(|v| arm.contains(v.as_str())).count();
            if named >= 2 {
                found.push(arm.to_string());
            }
            from = at + empty_body.len();
        }
    }
    found
}

/// Every `.rs` file under `tests/`, `tests/common/` included.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn no_test_accepts_a_failed_read_as_a_pass() {
    let dir = tests_dir();
    let sources = rust_sources(&dir);
    let names: Vec<String> = sources
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    // A scan that read nothing would pass silently, and a scan that
    // skipped its own file would be an exemption. Both are named here.
    assert!(
        names.iter().any(|n| n == "oracle_compat.rs"),
        "the scan did not reach oracle_compat.rs, the file this guard exists for; it found {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "oracle_verdicts.rs"),
        "the scan did not reach this file; a guard that exempts itself is a hole the size of itself. It found {names:?}"
    );
    assert!(
        sources.len() >= 10,
        "only {} test sources found under {}; the suite is larger than that, so the walk is broken",
        sources.len(),
        dir.display()
    );

    let mut offences = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for arm in arms_that_swallow_a_failure(&text) {
            offences.push(format!("{}: {arm}", path.display()));
        }
    }
    assert!(
        offences.is_empty(),
        "these arms let an oracle image that does not read back count as a pass. \
         Assert the outcome instead -- `assert!(matches!(outcome, ReadOutcome::Match), \"...\")` \
         -- or, if the layout really is unsupported, assert the specific error with its \
         number written down, so the test fails when reading breaks AND when support \
         arrives (#116):\n  {}",
        offences.join("\n  ")
    );
}

/// The detector's own tests. Each fixture is assembled from
/// `failure_variants()` for the same reason the scanner is: written out,
/// the fixtures would be offences in this file.
mod detector {
    use super::{arms_that_swallow_a_failure, code_only, failure_variants, without_comment};

    fn three_arm_source() -> String {
        let [open, lookup, read] = failure_variants();
        format!(
            "fn t() {{ match outcome {{ Outcome::Match => {{}} \
             Outcome::{open}(_) | Outcome::{lookup}(_) | Outcome::{read}(_) => {{}} \
             Outcome::Mismatch(s) => panic!(\"{{s}}\"), }} }}"
        )
    }

    #[test]
    fn the_shape_this_guard_exists_for_is_caught() {
        let found = arms_that_swallow_a_failure(&three_arm_source());
        assert_eq!(found.len(), 1, "expected exactly one offence: {found:?}");
    }

    #[test]
    fn the_shape_is_caught_when_it_is_written_across_lines() {
        let [open, lookup, read] = failure_variants();
        let source = format!(
            "match outcome {{\n    Outcome::Match => {{}}\n    Outcome::{open}(_)\n        | Outcome::{lookup}(_)\n        | Outcome::{read}(_) => {{ }}\n}}"
        );
        assert_eq!(arms_that_swallow_a_failure(&source).len(), 1);
    }

    #[test]
    fn the_empty_body_written_as_a_unit_is_caught_too() {
        let [open, lookup, _] = failure_variants();
        let source = format!("match o {{ O::{open}(_) | O::{lookup}(_) => (), }}");
        assert_eq!(arms_that_swallow_a_failure(&source).len(), 1);
    }

    #[test]
    fn an_arm_that_panics_is_not_an_offence() {
        let [open, lookup, read] = failure_variants();
        let source = format!(
            "match o {{ O::{open}(e) | O::{lookup}(e) | O::{read}(e) => panic!(\"{{e}}\"), }}"
        );
        assert!(arms_that_swallow_a_failure(&source).is_empty());
    }

    #[test]
    fn one_named_variant_doing_nothing_is_not_an_offence() {
        // A test that deliberately tolerates one specific outcome is a
        // decision, not a blanket. Two or more is the blanket.
        let [open, ..] = failure_variants();
        let source = format!("match o {{ O::Match => {{}} O::{open}(_) => {{}} }}");
        assert!(arms_that_swallow_a_failure(&source).is_empty());
    }

    #[test]
    fn a_neighbouring_arm_does_not_lend_its_variant_names() {
        let [open, lookup, _] = failure_variants();
        let source = format!(
            "match o {{ O::{open}(e) => panic!(\"{{e}}\"), O::{lookup}(e) => panic!(\"{{e}}\"), O::Match => {{}} }}"
        );
        assert!(
            arms_that_swallow_a_failure(&source).is_empty(),
            "the empty `Match` arm must not inherit the two arms before it"
        );
    }

    #[test]
    fn the_shape_quoted_in_a_comment_is_not_an_offence() {
        let source = format!("// {}\nfn t() {{}}", three_arm_source());
        assert!(
            arms_that_swallow_a_failure(&source).is_empty(),
            "a comment describing the shape is documentation, not code"
        );
    }

    #[test]
    fn a_comment_marker_inside_a_string_does_not_blind_the_scan() {
        let [open, lookup, _] = failure_variants();
        let source = format!(
            "let url = \"https://erofs.docs.kernel.org/\"; match o {{ O::{open}(_) | O::{lookup}(_) => {{}} }}"
        );
        assert_eq!(
            arms_that_swallow_a_failure(&source).len(),
            1,
            "the `//` in a URL is not the start of a comment"
        );
    }

    #[test]
    fn a_comment_is_cut_at_the_marker_and_a_string_is_kept() {
        assert_eq!(without_comment("let a = 1; // note"), "let a = 1; ");
        assert_eq!(
            without_comment("let u = \"a//b\"; let c = 2;"),
            "let u = \"a//b\"; let c = 2;"
        );
        assert_eq!(code_only("a\n   b   c\n"), "a b c");
    }
}
