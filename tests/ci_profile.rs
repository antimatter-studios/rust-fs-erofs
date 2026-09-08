//! The debug run that lets the PR gate see an overflow guards itself.
//!
//! `overflow-checks` is on in debug and off in release, so a defect
//! whose only symptom is an arithmetic overflow panic cannot be
//! observed by a release-only test run. This repository already runs a
//! debug suite -- `release.yml:71`, `cargo test --locked --all-targets`
//! -- and that is NOT the same fact as the pull-request gate being able
//! to see the defect: `release.yml` triggers on a version tag, after
//! the change has already merged. A wrapping bug merges green here and
//! surfaces only when someone else cuts the next release, detached from
//! the change and from the person who could have caught it.
//!
//! So `ci.yml` -- the workflow that actually gates a merge -- needs its
//! own debug run, and this file is what keeps it there.
//!
//! # Two halves, neither redundant
//!
//! | half | asks | cannot answer |
//! |---|---|---|
//! | the scans here | is the step still in `ci.yml`, asked to check, and not disabled from the manifest | whether the build it produces actually traps |
//! | `overflow_checks` in `src/lib.rs` | does this build trap a real `u64::MAX + 1` | whether it was supposed to; it cannot notice its own absence |
//!
//! Delete the step and the runtime probe never runs at all. Keep the
//! step but drop the variable and the probe runs, finds nothing to
//! check, and passes doing nothing. Keep both and put
//! `overflow-checks = false` under `[profile.test]` and the step is
//! present, running, green and blind. Each needs its own guard.
//!
//! # Why this is an integration test and not a module under `src/`
//!
//! Cargo discovers `tests/*.rs` on its own, so there is no declaration
//! anywhere that can be deleted to switch this off, and `Cargo.toml`
//! sets no `autotests = false`. A guard living as a file under `src/`
//! behind a `#[cfg(test)] mod` line has no such protection: lose the
//! one line and the file stays, compiles into nothing, and asserts
//! nothing, with no lint to say so. That happened once already on a
//! sibling repository's version of this fix -- a `git reset --hard`
//! took the `mod` line, the suite went green, and seven assertions
//! silently ceased to exist.
//!
//! The runtime probe in `src/lib.rs` is the deliberate exception, and
//! is inline in `lib.rs` for the same reason: it must be part of the
//! library target the debug step builds, and inline there is no
//! declaration to lose.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ci_yml() -> PathBuf {
    manifest_dir()
        .join(".github")
        .join("workflows")
        .join("ci.yml")
}

/// Read a file the guards depend on, or fail.
///
/// It panics rather than returning `None` on purpose. An
/// `if !path.exists() { return }` anywhere in this module would
/// reproduce the exact class of blindness the module exists to prevent:
/// an assertion that is present, runs, and cannot report the thing it
/// was written for. A missing workflow is a finding, not a skip.
fn read_or_panic(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must fail rather than skip: a \
             version of it that returned early here would be the same \
             blindness it exists to prevent.",
            path.display()
        )
    })
}

/// Every `cargo test` invocation in a workflow that would be compiled
/// with overflow checks on.
///
/// Four things disqualify a line, and each one is a way the guard could
/// otherwise be satisfied by something that does not actually build in
/// debug:
///
/// - it is a YAML comment. This is not defensive here, it is load
///   bearing: `ci.yml` quotes `cargo test --locked --lib` verbatim
///   inside the comment block that explains the step, so a scan that
///   ignored comments would still find it after the step itself had
///   been deleted, and would pass;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release`, or names a profile explicitly;
/// - it sets a `CARGO_PROFILE_*` variable, which can turn overflow
///   checks off for the dev or test profile from outside the manifest.
///
/// `cargo build --locked --release --bin mkfs_erofs`, in this
/// repository's `validate-mkfs-bin` job, is not a `cargo test` and is
/// not considered; the `fsck.erofs` steps beside it invoke no cargo at
/// all.
fn runs_with_overflow_checks(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            if !command.contains("cargo test") {
                return None;
            }
            if command.contains("--release") || command.contains("--profile") {
                return None;
            }
            if command.contains("CARGO_PROFILE_") {
                return None;
            }
            Some(command.to_string())
        })
        .collect()
}

/// WHAT ELSE DECIDES WHETHER A STEP GATES.
///
/// The first version of this guard matched the text of a `- run:` line
/// and never looked at anything else in the step. That is enough to
/// find the command and useless for deciding whether the command's
/// result is read. Measured against this repository's own workflow:
/// adding `if: false` to the step, or `continue-on-error: true`, left
/// every one of the guard's 31 tests green while the gate went blind.
/// A step that runs and whose result nothing reads is this
/// constellation's own named defect, reproduced inside the guard
/// written to prevent it.
///
/// So the list is enumerated first, rather than discovered one defeat
/// at a time. A `run:` step gates a pull request only if ALL of these
/// hold:
///
/// 1. the step carries no `if:` -- a false condition skips it;
/// 2. the step carries no `continue-on-error:` -- its failure is
///    discarded;
/// 3. its JOB carries no `if:` -- same reasoning, one level up;
/// 4. its JOB carries no `continue-on-error:`;
/// 5. the workflow's `on:` still includes `pull_request` -- a scan
///    scoped to `ci.yml` assumes `ci.yml` is what runs on a pull
///    request, and that is a fact about the file, not a given.
///
/// OVER-STRICT IS THE SAFE DIRECTION HERE, so 1 and 2 reject on the
/// key's PRESENCE rather than trying to evaluate it. `if: false`,
/// `if: ${{ false }}`, and an `if:` on an expression that happens to
/// evaluate false are distinct spellings, and this crate has already
/// been caught by four spellings of one manifest key -- enumerating
/// them is the losing game. A step that genuinely needs a condition
/// can be split out; a guard that tries to interpret conditions is a
/// guard with a new defeat every time GitHub adds syntax.
///
/// A `run:` whose value is a `|` block is joined into one string, so a
/// command inside a shell loop is seen whole rather than as fragments.
#[derive(Debug)]
struct Step {
    keys: Vec<String>,
    run: String,
}

#[derive(Debug)]
struct Job {
    keys: Vec<String>,
    steps: Vec<Step>,
}

#[derive(Debug)]
struct Workflow {
    triggers: String,
    jobs: Vec<Job>,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Structure a workflow far enough to answer the five questions above.
///
/// Deliberately conservative: anything this cannot place confidently is
/// left out, so an unparsed step is a step that does not count. The
/// failure direction is a guard that refuses a workflow it did not
/// understand, which is loud, rather than one that approves it.
fn parse_workflow(text: &str) -> Workflow {
    let mut triggers = String::new();
    let mut jobs: Vec<Job> = Vec::new();

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    // The `on:` block, taken verbatim up to the next top-level key.
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("on:") {
            triggers.push_str(l);
            triggers.push('\n');
            i += 1;
            while i < lines.len() && (lines[i].trim().is_empty() || indent_of(lines[i]) > 0) {
                triggers.push_str(lines[i]);
                triggers.push('\n');
                i += 1;
            }
            continue;
        }
        if l.starts_with("jobs:") {
            i += 1;
            break;
        }
        i += 1;
    }

    // Jobs: each is a key at indent 2 under `jobs:`.
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            i += 1;
            continue;
        }
        let ind = indent_of(line);
        if ind == 0 {
            break; // another top-level key; jobs are done
        }
        if ind != 2 || !line.trim_end().ends_with(':') {
            i += 1;
            continue;
        }
        // A job. Collect its keys and steps until the next indent-2 key.
        let mut job = Job {
            keys: Vec::new(),
            steps: Vec::new(),
        };
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            if !l.trim().is_empty() && indent_of(l) <= 2 && !l.trim_start().starts_with('#') {
                break;
            }
            let t = l.trim_start();
            if indent_of(l) == 4 && !t.starts_with('#') && !t.starts_with('-') {
                if let Some(key) = t.split(':').next() {
                    job.keys.push(key.trim().to_string());
                }
            }
            if indent_of(l) == 4 && t.starts_with("steps:") {
                i += 1;
                // Steps: list items at some indent > 4.
                let mut item_indent: Option<usize> = None;
                while i < lines.len() {
                    let sl = lines[i];
                    if !sl.trim().is_empty()
                        && indent_of(sl) <= 4
                        && !sl.trim_start().starts_with('#')
                    {
                        break;
                    }
                    let st = sl.trim_start();
                    if st.starts_with("- ") {
                        let this_indent = indent_of(sl);
                        if item_indent.is_none() {
                            item_indent = Some(this_indent);
                        }
                        if Some(this_indent) == item_indent {
                            // A new step. Its keys sit at this_indent + 2.
                            let key_indent = this_indent + 2;
                            let mut step = Step {
                                keys: Vec::new(),
                                run: String::new(),
                            };
                            // First key is on the `- ` line itself.
                            let mut cur = st.trim_start_matches("- ").to_string();
                            let mut in_run = false;
                            loop {
                                let key = cur.split(':').next().unwrap_or("").trim().to_string();
                                if !key.is_empty() && !key.starts_with('#') {
                                    step.keys.push(key.clone());
                                }
                                if key == "run" {
                                    in_run = true;
                                    let after =
                                        cur.split_once(':').map(|x| x.1).unwrap_or("").trim();
                                    if after != "|" && !after.is_empty() {
                                        step.run.push_str(after);
                                        step.run.push('\n');
                                        in_run = false;
                                    }
                                } else if in_run {
                                    in_run = false;
                                }
                                i += 1;
                                if i >= lines.len() {
                                    break;
                                }
                                let nl = lines[i];
                                if nl.trim().is_empty() {
                                    if in_run {
                                        continue;
                                    }
                                    continue;
                                }
                                let ni = indent_of(nl);
                                let nt = nl.trim_start();
                                if ni <= this_indent && !nt.starts_with('#') {
                                    break; // next step or end of steps
                                }
                                if in_run && ni > key_indent {
                                    step.run.push_str(nt);
                                    step.run.push('\n');
                                    continue;
                                }
                                if ni == key_indent && !nt.starts_with('#') {
                                    cur = nt.to_string();
                                    continue;
                                }
                                // Anything else (comments, deeper mapping
                                // under a non-run key) is skipped.
                            }
                            job.steps.push(step);
                            continue;
                        }
                    }
                    i += 1;
                }
                continue;
            }
            i += 1;
        }
        jobs.push(job);
    }

    Workflow { triggers, jobs }
}

/// Does this workflow still run on a pull request at all?
fn runs_on_pull_request(wf: &Workflow) -> bool {
    wf.triggers.contains("pull_request")
}

/// Keys whose presence on a step or job means its result does not gate.
const NON_GATING_KEYS: [&str; 2] = ["if", "continue-on-error"];

/// The run commands of steps that run in debug AND actually gate a
/// pull request -- without requiring the handshake.
///
/// The headline assertion used the line-based scan while only the
/// handshake assertion was step-aware, so under `if: false` the
/// headline PASSED and its failure message would have claimed the
/// pull-request gate could see an overflow when the step it names does
/// not run. Every defeat spelling still turned the suite red through
/// the other assertion, so this was a precision defect rather than a
/// hole -- but it left the "runs without --release" property verified
/// line-based, and defeatable if the handshake assertion were ever
/// weakened. Both halves are step-aware now. Found on the sibling
/// `rust-fs-btrfs` copy of this guard; this repository's copy was
/// merged before the correction existed, which is what
/// rust-fs-erofs#76 tracks.
fn gating_runs_with_overflow_checks(workflow: &str) -> Vec<String> {
    let wf = parse_workflow(workflow);
    if !runs_on_pull_request(&wf) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for job in &wf.jobs {
        if job
            .keys
            .iter()
            .any(|k| NON_GATING_KEYS.contains(&k.as_str()))
        {
            continue;
        }
        for step in &job.steps {
            if step
                .keys
                .iter()
                .any(|k| NON_GATING_KEYS.contains(&k.as_str()))
            {
                continue;
            }
            out.extend(runs_with_overflow_checks(&step.run));
        }
    }
    out
}

/// The run commands of steps that both cover the library in debug with
/// the handshake AND actually gate a pull request.
fn gating_runs_that_prove_the_build_traps(workflow: &str) -> Vec<String> {
    let wf = parse_workflow(workflow);
    if !runs_on_pull_request(&wf) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for job in &wf.jobs {
        if job
            .keys
            .iter()
            .any(|k| NON_GATING_KEYS.contains(&k.as_str()))
        {
            continue;
        }
        for step in &job.steps {
            if step
                .keys
                .iter()
                .any(|k| NON_GATING_KEYS.contains(&k.as_str()))
            {
                continue;
            }
            for command in debug_runs_that_prove_the_build_traps(&step.run) {
                out.push(command);
            }
        }
    }
    out
}

/// The debug runs that ask the build to prove it traps an overflow.
///
/// A subset of [`runs_with_overflow_checks`]: those which also set the
/// `EXPECT_OVERFLOW_CHECKS` handshake, so that
/// `overflow_checks::the_build_the_gate_asked_to_check_does_check`
/// performs an overflow and fails if the build let it through.
///
/// A run carrying the handshake but also `--release` is not counted,
/// because [`runs_with_overflow_checks`] has already excluded it. Such
/// a step is a misconfiguration and it fails loudly rather than
/// quietly: the checks are legitimately off in release, so the
/// assertion the handshake arms would fire there every time.
fn debug_runs_that_prove_the_build_traps(workflow: &str) -> Vec<String> {
    runs_with_overflow_checks(workflow)
        .into_iter()
        .filter(|command| command.contains("EXPECT_OVERFLOW_CHECKS=1"))
        .collect()
}

/// The guard. Reads the workflow this repository's pull requests are
/// gated by and refuses if nothing in it compiles the overflow checks.
///
/// `ci.yml` specifically, not every workflow -- see
/// [`a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard`],
/// which is the one repository-specific decision in this file.
#[test]
fn the_pr_gate_still_tests_in_a_profile_that_can_see_an_overflow() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);

    let debug_runs = gating_runs_with_overflow_checks(&workflow);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` in {} runs without `--release`, so a defect whose \
         only symptom is an arithmetic overflow panic can merge without the \
         PR gate ever seeing it. release.yml already runs a debug suite, and \
         that does not help: it triggers on a version tag, after the change \
         has merged. If the debug step in ci.yml looked redundant beside the \
         release ones, it is not -- see the comment above it.",
        path.display()
    );
}

/// The other half of the workflow scan: the step exists, but does it
/// ask the build anything?
///
/// # Why a handshake rather than more spellings
///
/// The manifest scan below reads `Cargo.toml` and asks whether a known
/// spelling of "overflow checks are off" is present. Several spellings
/// of the key were needed before it was right, and then routes turned
/// up that are not in that file at all: a
/// `CARGO_PROFILE_TEST_OVERFLOW_CHECKS` variable set at step or job
/// level in the workflow, and a `.cargo/config.toml`, which nothing
/// here reads. All of them leave the debug step present, running, green
/// and blind.
///
/// They are all the same shape: a scanner enumerating the ways a thing
/// can be disabled, in the places it happens to look. Another pass buys
/// the next one. So the question is put to the build instead -- perform
/// an overflow, see whether you are stopped -- and this test's job
/// shrinks to making sure the gate still asks it.
#[test]
fn the_debug_run_asks_the_build_to_prove_it_traps_overflows() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);

    let proving = gating_runs_that_prove_the_build_traps(&workflow);
    assert!(
        !proving.is_empty(),
        "no `cargo test` in {} runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1, so nothing checks whether the profile the \
         gate builds actually traps an arithmetic overflow. Reading \
         Cargo.toml is not enough: the checks can also be turned off by a \
         CARGO_PROFILE_TEST_OVERFLOW_CHECKS variable at step or job level, \
         or by a .cargo/config.toml, neither of which is in any file this \
         test reads. The handshake is what arms the one check that cannot be \
         fooled by where the setting lives.",
        path.display()
    );
}

/// THE DISTINCTION THIS REPOSITORY NEEDS THAT A PORTED COPY WOULD MISS.
///
/// A workflow carrying a checking debug run under a name other than
/// `ci.yml` -- `release.yml`, in this repository's own case -- must not
/// satisfy the guards above. Simulated here with `release.yml`'s actual
/// step shape: a plain `cargo test --locked --all-targets` with no
/// `EXPECT_OVERFLOW_CHECKS`, because that workflow was never asked to
/// carry the handshake and does not need to -- it already runs in
/// debug, unconditionally, so nothing there was ever blind.
///
/// The scenario worth pinning is the near miss: even a hypothetical
/// debug run in `release.yml` that DID set the handshake would not make
/// `ci.yml`'s own absence of one acceptable, because `release.yml`
/// triggers too late to gate a merge. Both shapes are asserted below.
///
/// This is why the scan is scoped to one file rather than globbed over
/// `.github/workflows/`. A scan across every workflow here would report
/// this repository's actual defect -- that no debug run gates a *pull
/// request* -- as already fixed, because `release.yml:71` has run in
/// debug all along. The guard's correctness therefore comes from WHICH
/// FILE it opens, not from the parser refusing these shapes, and that
/// is a fact worth pinning rather than leaving as a comment someone
/// could stop believing.
#[test]
fn a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard() {
    let release_yml_as_it_is = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --all-targets
      - run: cargo test --locked --all-targets -- --ignored
";
    assert_eq!(
        runs_with_overflow_checks(release_yml_as_it_is),
        vec![
            "- run: cargo test --locked --all-targets".to_string(),
            "- run: cargo test --locked --all-targets -- --ignored".to_string(),
        ],
        "release.yml's real steps ARE debug runs -- the parser counts them, \
         and the only reason they do not satisfy the guard is that the guard \
         never opens that file"
    );
    assert!(
        debug_runs_that_prove_the_build_traps(release_yml_as_it_is).is_empty(),
        "release.yml carries no handshake, and is not asked to"
    );

    let release_yml_with_a_handshake = "\
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets
";
    assert_eq!(
        debug_runs_that_prove_the_build_traps(release_yml_with_a_handshake),
        vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets".to_string()],
        "the parser itself would count this line too -- so widening the scan \
         to every workflow would silently stop catching this repository's \
         actual defect"
    );

    // And the real guards must be reading ci.yml, not one of these.
    let scanned = read_or_panic(&ci_yml());
    assert!(
        !debug_runs_that_prove_the_build_traps(&scanned).is_empty(),
        "the guards above must be satisfied by ci.yml's own content, not by \
         any of the strings in this test"
    );
}

/// The full dotted paths that switch overflow checks off for the
/// profile `cargo test` builds.
///
/// # This compares a whole path, because a key is not a word
///
/// The first version of this scan tracked the `[section]` and compared
/// the key to the literal `"overflow-checks"`. That reads correctly and
/// is defeated by ordinary TOML, because the same setting has several
/// spellings and cargo honours all of them without a warning. Measured
/// on a sibling repository with a runtime `u64::MAX + 1` unit test as
/// the probe -- `cargo test --locked --lib` EXIT=101 means the checks
/// are on, EXIT=0 means they are off, and `cargo metadata --no-deps`
/// was EXIT=0 for every one:
///
/// ```text
///   (nothing)                                          EXIT=101  on
///   [profile.test]  overflow-checks = false            EXIT=0    off
///   [profile.test]  "overflow-checks" = false          EXIT=0    off
///   [profile.test]  'overflow-checks' = false          EXIT=0    off
///   [profile]       test.overflow-checks = false       EXIT=0    off
/// ```
///
/// A bare key, a basic string, a literal string, and a dotted key that
/// puts the profile name on the key side where a section-matching scan
/// never looks. Four of those five defeated the first version, and each
/// leaves the debug step in `ci.yml` present, running, green and blind
/// -- the exact state the guard exists to refuse.
///
/// So the section and the key are joined into one path and normalised
/// per segment, and the comparison is against the whole thing. That
/// covers the spellings above, a quoted *section* (`["profile"."test"]`),
/// and a fully top-level dotted key with no section at all.
///
/// Only `profile.dev` and `profile.test` count. `cargo test` builds the
/// `test` profile, which inherits from `dev`, so either can disable the
/// checks in one line. `profile.release` is deliberately absent: the
/// checks are off there by default, that is what ships, and the release
/// steps exist to test what ships.
fn profiles_disabling_overflow_checks(manifest: &str) -> Vec<String> {
    /// Split a dotted TOML path and strip each segment's quoting, so
    /// that `"profile" . 'test'` and `profile.test` are one path.
    fn normalise(path: &str) -> String {
        path.split('.')
            .map(|segment| {
                segment
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim()
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    const DISABLED: [&str; 2] = [
        "profile.dev.overflow-checks",
        "profile.test.overflow-checks",
    ];

    let mut section = String::new();
    let mut found = Vec::new();
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or(raw).trim();
        if line.starts_with('[') {
            section = normalise(line.trim_matches(|c| c == '[' || c == ']'));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if value.trim() != "false" {
            continue;
        }
        let key = normalise(key);
        let path = if section.is_empty() {
            key
        } else {
            format!("{section}.{key}")
        };
        if DISABLED.contains(&path.as_str()) {
            found.push(path);
        }
    }
    found
}

/// The half of the property the workflow scans cannot see.
///
/// A debug step in `ci.yml` only buys anything while the profile it
/// builds actually checks. One line -- `overflow-checks = false` under
/// `[profile.test]`, or under this repository's existing
/// `[profile.dev]`, a plausible way to make a slow suite faster --
/// would leave that step present, running, green, and no longer able to
/// observe an overflow, with every workflow assertion above still
/// passing. A guard for half a condition is the defect it was written
/// to prevent.
///
/// The runtime probe in `src/lib.rs` would also catch this. This scan
/// is kept as defence in depth: it fails earlier in the gate and names
/// the offending manifest key, which is a better diagnostic than "the
/// build did not trap".
#[test]
fn the_profile_that_cargo_test_builds_still_checks_for_overflow() {
    let path = manifest_dir().join("Cargo.toml");
    let manifest = read_or_panic(&path);

    let disabled = profiles_disabling_overflow_checks(&manifest);
    assert!(
        disabled.is_empty(),
        "{} sets `overflow-checks = false` under {disabled:?}. `cargo test` \
         builds the `test` profile, which inherits from `dev`, so this \
         switches off the check that the debug step in ci.yml exists to run \
         -- leaving that step present, green, and blind. Put it back, or the \
         debug step is costing a compile and buying nothing.",
        path.display()
    );
}

/// The workflow parser is the part of this that can rot, so it is
/// checked against each shape it has to tell apart.
mod parser {
    use super::runs_with_overflow_checks;

    /// The trap this repository actually contains. `ci.yml` documents
    /// the debug step by quoting the command, so the text survives the
    /// step's deletion.
    #[test]
    fn a_debug_run_quoted_in_a_comment_does_not_count() {
        let quoted_in_a_comment = "\
jobs:
  test:
    steps:
      # Measured on this branch:
      #     cargo test --locked --release --lib   ->  EXIT=0
      #     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib   ->  EXIT=101
      - run: cargo test --locked --release
";
        assert_eq!(
            runs_with_overflow_checks(quoted_in_a_comment),
            Vec::<String>::new(),
            "a debug command quoted inside a comment is documentation, not a run"
        );
    }

    #[test]
    fn a_real_debug_run_counts() {
        let with_the_step = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --release
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            runs_with_overflow_checks(with_the_step),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A step whose command is `--release` but which carries a trailing
    /// comment mentioning the debug run.
    #[test]
    fn a_trailing_comment_does_not_promote_a_release_run() {
        let inline = "      - run: cargo test --locked --release  # not cargo test --lib\n";
        assert_eq!(
            runs_with_overflow_checks(inline),
            Vec::<String>::new(),
            "the command is --release; the comment after it is not a second run"
        );
    }

    /// The inline-comment strip, which nothing else here pins. A real
    /// debug run whose trailing comment happens to contain `--release`
    /// must still be counted. Without the strip that word disqualifies
    /// the command, and the guard then fails insisting there is no
    /// debug run while one is sitting in front of it.
    #[test]
    fn a_trailing_comment_naming_release_does_not_disqualify_a_debug_run() {
        let line = "      - run: cargo test --locked --lib  # deliberately not --release\n";
        assert_eq!(
            runs_with_overflow_checks(line),
            vec!["- run: cargo test --locked --lib".to_string()],
            "the command is a debug run; --release appears only in its comment"
        );
    }

    /// The ways a run can carry no `--release` and still be built
    /// without the checks.
    #[test]
    fn a_profile_named_another_way_does_not_count() {
        let lines = [
            "      - run: cargo test --locked --profile release-with-debug --lib",
            "      - run: CARGO_PROFILE_TEST_OVERFLOW_CHECKS=false cargo test --locked --lib",
            "      - run: CARGO_PROFILE_DEV_OVERFLOW_CHECKS=false cargo test --locked --lib",
        ];
        for line in lines {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} does not compile the overflow checks"
            );
        }
        assert_eq!(
            lines.len(),
            3,
            "the loop above must have examined every shape"
        );
    }

    /// `cargo build` is not `cargo test`. This repository's
    /// `validate-mkfs-bin` job builds the mkfs binary and then runs
    /// `fsck.erofs` against its output; none of that is a test run, and
    /// a parser that counted `cargo build --release` lines would be
    /// looking at the wrong steps entirely.
    #[test]
    fn a_cargo_build_step_is_not_a_test_run() {
        let validate_job = "\
jobs:
  validate-mkfs-bin:
    steps:
      - run: cargo build --locked --release --bin mkfs_erofs
      - run: fsck.erofs --extract=/tmp/extracted /tmp/out.erofs
";
        assert_eq!(
            runs_with_overflow_checks(validate_job),
            Vec::<String>::new(),
            "building a binary is not running a test suite"
        );
    }
}

/// The handshake half of the workflow parser.
mod handshake {
    use super::debug_runs_that_prove_the_build_traps;

    #[test]
    fn a_debug_run_carrying_the_handshake_counts() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A debug run that exists and asks the build nothing. Buys a
    /// compile and no information.
    #[test]
    fn a_debug_run_without_the_handshake_does_not_count() {
        let yaml = "      - run: cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            Vec::<String>::new(),
            "the step is there but nothing checks the build it produced"
        );
    }

    /// A handshake on a release run proves nothing and must not satisfy
    /// this: the checks are off in release on purpose.
    #[test]
    fn the_handshake_on_a_release_run_does_not_count() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --release\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            Vec::<String>::new(),
        );
    }

    /// And quoted inside the comment block that explains it, which is
    /// where `ci.yml` also mentions it.
    #[test]
    fn the_handshake_quoted_in_a_comment_does_not_count() {
        let yaml = "      #     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            Vec::<String>::new(),
        );
    }
}

/// The manifest scanner, held to the shapes it has to tell apart. These
/// do not depend on this repository's own `Cargo.toml`, so they keep
/// meaning something after it changes.
mod manifest_parser {
    use super::profiles_disabling_overflow_checks;

    #[test]
    fn the_test_profile_disabling_the_checks_is_caught() {
        let manifest = "\
[profile.release]
lto = true

[profile.test]
overflow-checks = false
";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// This repository has an explicit `[profile.dev]`, so this is the
    /// likeliest place the setting would actually arrive.
    #[test]
    fn the_dev_profile_disabling_the_checks_is_caught() {
        let manifest = "[profile.dev]\nopt-level = 1\noverflow-checks   =   false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.dev.overflow-checks".to_string()],
        );
    }

    /// Release is expected to have them off. Flagging it would make the
    /// guard fail on every correct manifest, which is the fastest way
    /// to get a guard deleted.
    #[test]
    fn the_release_profile_disabling_the_checks_is_not_flagged() {
        let manifest = "[profile.release]\noverflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// A commented-out line is not a setting -- the same trap as the
    /// workflow parser's, in the other file this module reads.
    #[test]
    fn a_commented_out_setting_is_not_a_setting() {
        let manifest = "[profile.test]\n# overflow-checks = false\nopt-level = 1\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// The comment strip, which nothing else here pins. The realistic
    /// way this setting arrives is with its excuse on the same line,
    /// and it must still be caught: unstripped, the value reads
    /// `false  # speeds the suite up`, which is not `false`, and the
    /// guard waves through the exact edit it exists to catch.
    #[test]
    fn a_disabling_line_with_a_trailing_comment_is_still_caught() {
        let manifest = "[profile.test]\noverflow-checks = false  # speeds the suite up\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// A different setting being `false` is not this setting being
    /// `false`. Without this the scanner could be keying on the value
    /// alone -- flagging any `= false` under those two sections -- and
    /// every other test here would still pass.
    #[test]
    fn another_setting_being_false_is_not_this_one() {
        let manifest = "[profile.test]\ndebug-assertions = false\nopt-level = 1\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// THE SPELLINGS THAT DEFEATED THE FIRST VERSION. Each of these was
    /// measured to genuinely switch the checks off, with no warning
    /// from cargo -- see the table on
    /// `profiles_disabling_overflow_checks`. A guard that reads one
    /// spelling of a setting is a guard against typing it one way.
    #[test]
    fn a_double_quoted_key_is_the_same_key() {
        let manifest = "[profile.test]\n\"overflow-checks\" = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn a_literal_quoted_key_is_the_same_key() {
        let manifest = "[profile.dev]\n'overflow-checks' = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.dev.overflow-checks".to_string()],
        );
    }

    /// The one a section-matching scan cannot see at all: the profile
    /// name is on the key side, so the section is only `profile`.
    #[test]
    fn a_dotted_key_putting_the_profile_on_the_key_side_is_caught() {
        let manifest = "[profile]\ntest.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// And with no section header at all, which is still valid TOML.
    #[test]
    fn a_top_level_dotted_key_is_caught() {
        let manifest = "profile.test.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn a_quoted_section_is_the_same_section() {
        let manifest = "[\"profile\".'test']\noverflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// Release stays exempt in the dotted spelling too, or normalising
    /// the path would have quietly widened what the guard refuses.
    #[test]
    fn the_release_profile_is_exempt_in_the_dotted_spelling_too() {
        let manifest = "[profile]\nrelease.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// `true` is the state we want and must not be reported as the
    /// state we do not. Without this the scanner could be keying on the
    /// word `overflow-checks` alone and nothing here would notice.
    #[test]
    fn enabling_the_checks_explicitly_is_not_flagged() {
        let manifest = "[profile.test]\noverflow-checks = true\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }
}

/// WHAT ELSE DECIDES WHETHER THE STEP GATES -- one test per item on the
/// enumerated list, because each is a separate way for the gate to go
/// blind with the command still present and still matching.
///
/// The version of this guard these replace matched the `- run:` line in
/// isolation. Measured against this repository's own workflow, `if:
/// false` and `continue-on-error: true` each left all 31 of its tests
/// green while the gate stopped gating.
mod gating {
    use super::gating_runs_that_prove_the_build_traps;

    /// The shape that does gate, as a control. Every test below is this
    /// with one thing added, so a failure here would mean the fixture
    /// is wrong rather than the property.
    const GATING: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";

    #[test]
    fn the_control_shape_gates() {
        assert_eq!(
            gating_runs_that_prove_the_build_traps(GATING).len(),
            1,
            "the control must be counted, or every test below passes for the wrong reason"
        );
    }

    #[test]
    fn a_step_carrying_if_does_not_gate() {
        for condition in [
            "if: false",
            "if: ${{ false }}",
            "if: github.event_name == 'push'",
            "if: ${{ env.SOMETHING == 'yes' }}",
        ] {
            let yaml = GATING.replace(
                "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                &format!(
                    "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n        {condition}\n"
                ),
            );
            assert!(
                gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
                "a step carrying `{condition}` may or may not run, so it cannot be what \
                 makes the gate able to see an overflow. Rejected on the key's presence \
                 rather than by evaluating it -- the spellings are open-ended."
            );
        }
    }

    #[test]
    fn a_step_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n        continue-on-error: true\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "the step runs and its failure is discarded, which is the project's own named \
             defect: a step that runs and whose result nothing reads"
        );
    }

    #[test]
    fn a_job_carrying_if_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    if: false\n");
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "the same reasoning one level up: a job that may not run cannot gate"
        );
    }

    #[test]
    fn a_job_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    continue-on-error: true\n");
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "a job whose failure is discarded cannot gate, however sound its steps"
        );
    }

    /// The assumption the `ci.yml`-only scan rests on, which is a fact
    /// about the file rather than a given.
    #[test]
    fn a_workflow_that_no_longer_runs_on_pull_request_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  push:\n    branches: [main]\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "scoping the scan to ci.yml assumes ci.yml is what runs on a pull request; if its \
             triggers stop including pull_request, the step gates nothing no matter how it looks"
        );
    }

    /// A `run: |` block is read whole, so a command inside a loop is
    /// visible. This repository has TWO such loops in kernel-gate, and
    /// a line-range extraction drops the second.
    #[test]
    fn a_run_block_is_read_whole() {
        let yaml = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - name: a block
        run: |
          set -euo pipefail
          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            gating_runs_that_prove_the_build_traps(yaml).len(),
            1,
            "a command inside a `run: |` block must be seen; the kernel-gate loops live in \
             blocks like this one"
        );
    }
}
