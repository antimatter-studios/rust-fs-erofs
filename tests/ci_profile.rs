//! The debug run that lets the PR gate see an overflow guards itself.
//!
//! `overflow-checks` is on in debug and off in release, so a defect
//! whose only symptom is an arithmetic overflow panic cannot be
//! observed by a release-only test run. `release.yml` runs a debug suite
//! too -- its `chore test` starts with `task: test:unit` -- and that is
//! NOT the same fact as the pull-request gate being able to see the
//! defect: `release.yml` triggers on a version tag, after the change
//! has already merged. A wrapping bug merges green here and surfaces
//! only when someone else cuts the next release, detached from the
//! change and the person who could have caught it.
//!
//! So `ci.yml` -- the workflow that actually gates a merge -- needs its
//! own debug run, and this file is what keeps it there. It checks
//! `ci.yml` specifically and is not satisfied by `release.yml` having
//! one; see `a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard`
//! for that distinction pinned as a test rather than left as a comment
//! someone could stop believing.
//!
//! # The run lives in `chores.yml`, one indirection away
//!
//! Every CI job runs chore tasks, so a green local `chore test` is the
//! same evidence as a green CI run. The debug run is therefore not
//! spelled in `ci.yml` at all: the `unit` job runs `chore test:unit`,
//! and it is `chores.yml`'s `test:unit` task that carries
//! `EXPECT_OVERFLOW_CHECKS=1` and the `cargo test` (through
//! `scripts/test.sh`, which ends in `cargo test --features cli "$@"`). The guard FOLLOWS
//! THAT INDIRECTION rather than accepting the task's name: a `chore
//! <task>` line in a gating step is resolved in `chores.yml`, through any
//! `task:` items, and the commands found there are held to the same rules
//! as a command written in the step. See [`chore_checking_debug_runs`]
//! for the ways a task stops gating (chore may skip it as up to date, or
//! discard its failure).
//!
//! The same indirection carries the rest of the gate: which jobs run on
//! which architecture, where the fixtures come from (the
//! fs-linux-test-harness VM, under KVM, on the x86_64 `fixtures` job --
//! GitHub's arm64 runners have no KVM), and that `ci-ok` waits on every
//! job. Those are pinned by
//! `the_pr_gate_builds_fixtures_once_in_the_harness_vm_and_tests_both_architectures_through_chore`.
//!
//! # Why this is an integration test and not a module under `src/`
//!
//! Cargo discovers `tests/*.rs` on its own, so there is no declaration
//! anywhere that can be deleted to switch this off. A guard living as a
//! file under `src/` behind a `#[cfg(test)] mod` line has no such
//! protection: lose the one line and the file stays, compiles into
//! nothing, and asserts nothing, with no lint to say so. That happened
//! once already on a sibling repository's version of this fix.
//!
//! # The other half: does the debug run actually ask anything
//!
//! A workflow or task file quoting a debug command inside the comment
//! explaining it means a scan
//! that ignored comments would keep passing after the step itself was
//! deleted. And a debug step that compiles but never checks anything is
//! costing a compile for nothing, so the scan also requires the
//! `EXPECT_OVERFLOW_CHECKS` handshake that arms
//! `overflow_checks::the_build_the_gate_asked_to_check_does_check` in
//! `src/lib.rs` -- the runtime half that actually performs an overflow
//! and fails if the build let it through. This file proves the step is
//! present and asked to check; it cannot prove the build can see the
//! overflow, which is what the runtime test is for. Neither is
//! redundant with the other: delete the step and the runtime test never
//! runs at all; keep the step but drop the variable and the runtime
//! test runs, finds nothing to check, and passes doing nothing.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Read a file the guard depends on, or fail.
///
/// Panics rather than returning `None` on purpose: a version of this
/// that skipped when the file was missing would reproduce the exact
/// blindness the guard exists to prevent.
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
/// with overflow checks on AND asked to check for one.
///
/// Three things disqualify a line:
///
/// - it is a YAML comment. Load-bearing here, not defensive: `ci.yml`
///   quotes the debug command verbatim in the comment above the step
///   that runs it;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release`, or names a profile explicitly.
///
/// And the run must carry `EXPECT_OVERFLOW_CHECKS=1` -- a debug step
/// that never asks the build anything buys nothing over deleting it.
///
/// `scripts/test.sh` IS `cargo test`, and is read as one. It picks a
/// scratch directory and then runs `cargo test --features cli "$@"` (inside the
/// harness's `vm.sh session` when the harness is there), so every argument
/// it is given -- `--release`, `-r` -- is cargo's, and the rules above
/// apply to it word for word. `chores.yml` runs the suite through it.
/// That the script still ends that way is pinned by
/// `scripts_test_sh_is_still_cargo_test_with_its_arguments`: if it
/// stopped passing its arguments through, reading it as `cargo test`
/// would be a lie this guard tells.
fn checking_debug_runs(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            let as_cargo = command.replace(TEST_WRAPPER, "cargo test");
            if !as_cargo.contains("cargo test") {
                return None;
            }
            if as_cargo.contains("--release")
                || as_cargo.contains("--profile")
                || selects_release_by_short_flag(&as_cargo)
            {
                return None;
            }
            if !as_cargo.contains("EXPECT_OVERFLOW_CHECKS=1") {
                return None;
            }
            Some(command.to_string())
        })
        .collect()
}

/// The repository's `cargo test` wrapper. See [`checking_debug_runs`].
const TEST_WRAPPER: &str = "scripts/test.sh";

/// Whether `command` runs `cargo test` with the release profile selected
/// by its short flag (#158).
///
/// `cargo test -r` is `cargo test --release`, and the string check above
/// does not see it. Nor is it one spelling: clap merges short flags, so
/// `-qr` and `-rq` carry it as well, anywhere before `--`. A cluster ends
/// at a short option that takes a value -- `-p`, `-j`, `-F` or `-Z` --
/// whose value is the rest of the word or, when the word ends there, the
/// next one: `-j4 -r` is release, `-pr` names a package `r`. Everything
/// after `--` belongs to the test harness, where `-r` is not cargo's.
fn selects_release_by_short_flag(command: &str) -> bool {
    shell_commands(command).iter().any(|command| {
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        (0..words.len()).any(|at| {
            // `cargo` by name or by path, then any `+toolchain`, then `test`.
            let is_cargo = words[at] == "cargo" || words[at].ends_with("/cargo");
            let mut next = at + 1;
            while is_cargo && words.get(next).is_some_and(|w| w.starts_with('+')) {
                next += 1;
            }
            is_cargo && words.get(next) == Some(&"test") && release_in(&words[next + 1..])
        })
    })
}

/// `command` split into the commands the shell's control operators --
/// `&&`, `||`, `;`, `|` and `&` -- separate, each as its words, with
/// quotes and backslashes removed as the shell removes them.
///
/// Operators need no spaces: `true&&cargo test -r` is a `cargo test` run,
/// and in `cargo test --lib&&rm -rf build` the `-rf` is `rm`'s. An `&` or
/// `|` straight after `>` or `<` is part of a redirection (`2>&1`, `>|`).
/// Inside quotes, or after a backslash, nothing is an operator or a word
/// break, and what the quotes held is the word: `--features 'a;b' -r` is
/// one command, and `'-r'` is `-r`.
fn shell_commands(command: &str) -> Vec<Vec<String>> {
    let mut commands = vec![Vec::new()];
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    let mut previous = None;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let word = word.get_or_insert_with(String::new);
                word.extend(chars.by_ref().take_while(|&q| q != '\''));
            }
            '"' => {
                let word = word.get_or_insert_with(String::new);
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            word.extend(chars.next());
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => word.get_or_insert_with(String::new).extend(chars.next()),
            ';' | '&' | '|' if !matches!(previous, Some('>' | '<')) => {
                if c != ';' && chars.peek() == Some(&c) {
                    chars.next();
                }
                commands.last_mut().unwrap().extend(word.take());
                commands.push(Vec::new());
            }
            c if c.is_whitespace() => commands.last_mut().unwrap().extend(word.take()),
            c => word.get_or_insert_with(String::new).push(c),
        }
        previous = Some(c);
    }
    commands.last_mut().unwrap().extend(word.take());
    commands
}

/// Whether `cargo test`'s `arguments`, up to the end of its own command,
/// carry `-r`. See [`selects_release_by_short_flag`].
///
/// `arguments` are one command's words ([`shell_commands`]), so in
/// `cargo test --lib && rm -rf build` the `r` in `-rf` is not among them.
fn release_in(arguments: &[&str]) -> bool {
    const LONG_OPTIONS_TAKING_A_VALUE: [&str; 15] = [
        "--package",
        "--exclude",
        "--features",
        "--target",
        "--target-dir",
        "--manifest-path",
        "--profile",
        "--test",
        "--bin",
        "--example",
        "--bench",
        "--jobs",
        "--message-format",
        "--color",
        "--config",
    ];
    let mut next_is_a_value = false;
    for &argument in arguments {
        if std::mem::take(&mut next_is_a_value) {
            continue;
        }
        if argument == "--" {
            return false;
        }
        if argument.starts_with("--") {
            next_is_a_value =
                !argument.contains('=') && LONG_OPTIONS_TAKING_A_VALUE.contains(&argument);
        } else if let Some(cluster) = argument.strip_prefix('-') {
            for (at, flag) in cluster.char_indices() {
                match flag {
                    'r' => return true,
                    'p' | 'j' | 'F' | 'Z' => {
                        next_is_a_value = at + 1 == cluster.len();
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// A workflow, structured just far enough to answer one question:
/// does this step's result actually gate a pull request?
///
/// The line-based scan above finds the command. It cannot see the
/// step's sibling keys, so `if: false` and `continue-on-error: true`
/// left every guard test green while the gate stopped gating -- a step
/// that runs and whose result nothing reads, which is this project's
/// own named defect, committed inside the guard written to prevent it.
///
/// The conditions are ENUMERATED rather than patched one defeat at a
/// time, because twice on this shape the defeat lived in what the scan
/// does not look at rather than in what it compares. A `run:` step
/// gates a pull request only if the step carries no `if:` and no
/// `continue-on-error:`, its job carries neither either, and the
/// workflow still triggers on `pull_request`.
///
/// `if:` and `continue-on-error:` are rejected on the KEY'S PRESENCE,
/// not by evaluating it. `if: false`, `if: ${{ false }}` and an `if:`
/// on an expression that happens to be false are distinct spellings,
/// and four spellings of one manifest key had already defeated a
/// matcher on a sibling repository -- enumerating them is the losing
/// game. Over-strict is the safe direction: a step that genuinely
/// needs a condition can be split out, whereas a guard that
/// interprets conditions acquires a new defeat whenever the syntax
/// grows.
///
/// A step's other keys -- `name:`, `env:`, `uses:`/`with:` -- say
/// nothing about whether the result is read, so they are ACCEPTED. An
/// `env:` mapping in particular must not disqualify a step: that would
/// be over-strictness in the one direction that costs something, since
/// the handshake this guard looks for is itself an environment
/// variable and a maintainer may reasonably move it into a mapping.
///
/// # WHY THIS IS PARSED AND NO LONGER SCANNED
///
/// The version this replaces hand-rolled the YAML, and it was correct
/// only in the sense that it had been patched five times. Each patch
/// was a helper taught one more piece of ordinary grammar:
///
/// ```text
///   without_comment        a trailing `#`, so a commented-out trigger
///                          stopped counting as a trigger
///   key_of                 quotes, so `"if": false` stopped being a
///                          different key from `if: false`
///   opens_a_block_scalar   `|-`, `|+`, `>`, `>-`, `>+`, `|2`, `>2-`,
///                          so a block's contents were read at all
///   indent_of              the block structure itself
///   triggers: Vec<String>  whole names, so `pull_request_review` and
///                          `pull_request` stopped being the same
/// ```
///
/// Every one of those is a rule a YAML parser already has. And the
/// cost of learning them by hand is recorded in this file, twice over:
/// `key_of`'s own comment noted that the identical quote-normalisation
/// had already been added to `profiles_disabling_overflow_checks` a
/// few dozen lines above, after a quoted `"overflow-checks" = false`
/// defeated that scan -- the lesson did not travel between two parsers
/// in one file. Learning it a sixth time was the alternative to this.
///
/// The properties those helpers defended are not dropped with them.
/// Each is now asserted in `mod gating` against the parser instead:
/// every block scalar style is read whole, a quoted key is the same
/// key, a commented-out trigger is not a trigger, and a `#` inside a
/// quoted shell value is content rather than a comment.
///
/// `saphyr` is a dev-dependency, so nothing here reaches a consumer of
/// the crate.
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
    /// The trigger NAMES, parsed. Not the `on:` block's text: a
    /// substring search over that text answered `true` for
    /// `pull_request_review:` and for `pull_request` sitting inside a
    /// comment, so the guard reported pull-request coverage that was
    /// not there. Neither spelling needs an adversarial author.
    triggers: Vec<String>,
    jobs: Vec<Job>,
}

/// The value of `name` in a YAML mapping, or `None`.
///
/// By name rather than by constructing a key, because `saphyr`'s `Yaml`
/// borrows the source text and building one to hand to `get` is more
/// ceremony than the lookup is worth here.
fn field<'a, 'b>(node: &'a Yaml<'b>, name: &str) -> Option<&'a Yaml<'b>> {
    node.as_mapping()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// The keys of a YAML mapping, as plain strings.
///
/// The parser has already resolved the quoting, so `"if"`, `'if'` and
/// `if` all arrive here as `if`. That is the whole of what `key_of`
/// did: there is no un-quoting step left to forget.
fn keys_of(node: &Yaml) -> Vec<String> {
    node.as_mapping()
        .map(|mapping| {
            mapping
                .iter()
                .filter_map(|(key, _)| key.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Structure a workflow far enough to answer the questions above.
///
/// Panics on a workflow it cannot parse, deliberately. A guard that
/// returned an empty `Workflow` for a file it did not understand would
/// report "no debug run gates this" -- a failure, so that direction is
/// safe -- but one that returned early with a PASS would be the
/// blindness this module exists to prevent. Failing on the parse error
/// names the real problem instead of a consequence of it.
fn parse_workflow(text: &str) -> Workflow {
    let documents = Yaml::load_from_str(text).unwrap_or_else(|e| {
        panic!(
            "workflow is not valid YAML: {e}. This guard reads the workflow \
             rather than scanning its text, so a file it cannot parse is a \
             failure and never a pass."
        )
    });
    let Some(document) = documents.first() else {
        return Workflow {
            triggers: Vec::new(),
            jobs: Vec::new(),
        };
    };

    // `on:` takes three legal shapes: a mapping of trigger names, a
    // sequence of them, or a single scalar. All three are names.
    //
    // Note that `on` survives as the string key `on` and is not folded
    // into the boolean `true` -- saphyr implements the YAML 1.2 core
    // schema, where only `true`/`false` are booleans. The YAML 1.1
    // reading that would break every GitHub workflow ever written does
    // not apply.
    let triggers = match field(document, "on") {
        Some(on) if on.as_mapping().is_some() => keys_of(on),
        Some(on) if on.as_sequence().is_some() => on
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(on) => on.as_str().map(str::to_string).into_iter().collect(),
        None => Vec::new(),
    };

    let mut jobs = Vec::new();
    if let Some(mapping) = field(document, "jobs").and_then(Yaml::as_mapping) {
        for (_, body) in mapping.iter() {
            let steps = field(body, "steps")
                .and_then(Yaml::as_sequence)
                .into_iter()
                .flatten()
                .map(|step| Step {
                    keys: keys_of(step),
                    // A `run:` block of any style -- `|`, `|-`, `|+`,
                    // `>`, `>-`, `>+`, `|2`, `>2-` -- arrives as one
                    // string with the block folded per its own rules,
                    // so a command inside a shell loop is seen whole
                    // rather than as fragments, and no style is
                    // mistaken for the command itself. That is what
                    // `opens_a_block_scalar` enumerated by hand.
                    run: field(step, "run")
                        .and_then(Yaml::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            jobs.push(Job {
                keys: keys_of(body),
                steps,
            });
        }
    }

    Workflow { triggers, jobs }
}

/// Does this workflow still run on a pull request at all?
///
/// The assumption the `ci.yml`-only scope rests on, and a fact about
/// the file rather than a given: if the triggers stop including
/// `pull_request`, the step gates nothing however it looks.
///
/// MATCHED WHOLE, against parsed trigger names. A substring search
/// over the `on:` block's text answered `true` for
/// `pull_request_review:` -- which fires on review events, not on a
/// pull request opening or being pushed to, so it gates nothing -- and
/// for `pull_request` inside a comment, including the comment that
/// says it was switched off.
///
/// # `pull_request_target` is NOT accepted, and that is a change
///
/// This guard used to accept it, on the reasoning that it also runs on
/// pull requests and can be a required check. That was rust-fs-ext4#149. It runs
/// against the BASE repository with a write token and the repository's
/// secrets, and checks out the base ref by default, so a workflow
/// triggered only that way may never build the contributor's code at
/// all -- and accepting it as proof the merge is gated is permissive
/// in the worst direction for twelve library crates that take pull
/// requests from forks.
///
/// The alternative considered was to accept it conditionally, on
/// finding a checkout that names the pull request head. Both designs
/// refuse when they do not recognise the checkout, so both fail safe;
/// what settled it is that `pull_request_target` appears in ZERO of
/// the twelve repositories' workflows. The conditional branch would
/// guard a configuration that exists nowhere, and "we do not use this
/// trigger, and a test says so" is the better standing statement.
///
/// Note the narrowness of what this refuses: a workflow carrying BOTH
/// `pull_request:` and `pull_request_target:` -- the ordinary way to
/// reach secrets without giving up the gate -- is satisfied by the
/// former and never reaches this question.
fn runs_on_pull_request(wf: &Workflow) -> bool {
    wf.triggers.iter().any(|t| t == "pull_request")
}

/// The branch names a `pull_request:` trigger is restricted to, if any
/// (#132).
///
/// `on: pull_request: branches: [main]` means a pull request based on
/// anything else runs **no** CI — and still collects its review-bot
/// ticks, so it reads as verified. This repository stacks pull requests
/// routinely, which is exactly the case that got nothing: one check
/// against another PR's three on the same day.
fn pull_request_branch_filter(document: &Yaml) -> Vec<String> {
    let Some(on) = field(document, "on").or_else(|| field(document, "true")) else {
        return Vec::new();
    };
    let Some(pr) = field(on, "pull_request") else {
        return Vec::new();
    };
    let Some(branches) = field(pr, "branches") else {
        return Vec::new();
    };
    branches
        .as_sequence()
        .map(|seq| {
            seq.iter()
                .filter_map(|b| b.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Why `workflow` gates no pull request at all, or `None` if it does.
///
/// The real-file assertions ask this FIRST. Without it, a workflow whose
/// `on:` block moved was reported as having no debug `cargo test` in a
/// step that gates a pull request, which sends the reader to a step that
/// is fine (#155). This names the triggers that were found instead, and
/// says why `pull_request_target` alone does not count.
fn not_a_pull_request_gate(workflow: &str) -> Option<String> {
    let wf = parse_workflow(workflow);
    if runs_on_pull_request(&wf) {
        return None;
    }
    let mut why = format!(
        "the workflow does not trigger on `pull_request` at all (its triggers: {:?}), so \
         none of its steps gates a pull request however they are written. The steps are \
         not the problem; the `on:` block is.",
        wf.triggers
    );
    if wf.triggers.iter().any(|t| t == "pull_request_target") {
        why.push_str(
            " `pull_request_target` alone is refused on purpose: it runs against the base \
             repository and may never build the contributor's code. See \
             `runs_on_pull_request`; carry `pull_request:` beside it.",
        );
    }
    Some(why)
}

/// Keys whose presence on a step or job means its result does not gate.
const NON_GATING_KEYS: [&str; 2] = ["if", "continue-on-error"];

fn carries_a_non_gating_key(keys: &[String]) -> bool {
    keys.iter().any(|k| NON_GATING_KEYS.contains(&k.as_str()))
}

/// Walk a workflow's steps and collect what `select` finds in each
/// `run:`.
///
/// `gating` restricts the walk to steps whose result the pull-request
/// gate actually reads: the workflow must still trigger on a pull
/// request, and neither the job nor the step may carry a key from
/// [`NON_GATING_KEYS`].
///
/// One walk, shared by both halves of the guard. BOTH HALVES ARE
/// STEP-AWARE, and that is deliberate. On the sibling `rust-fs-btrfs`
/// copy of this guard the headline assertion was left line-based while
/// only the handshake one was step-aware, so under `if: false` the
/// headline PASSED and its own failure message would have claimed the
/// pull-request gate could see an overflow when the step it names does
/// not run. Sharing the walk is what stops the two drifting apart
/// again, rather than fixing them separately twice.
fn scan_steps(workflow: &str, gating: bool, select: &dyn Fn(&str) -> Vec<String>) -> Vec<String> {
    let wf = parse_workflow(workflow);
    if gating && !runs_on_pull_request(&wf) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for job in &wf.jobs {
        if gating && carries_a_non_gating_key(&job.keys) {
            continue;
        }
        for step in &job.steps {
            if gating && carries_a_non_gating_key(&step.keys) {
                continue;
            }
            out.extend(select(&step.run));
        }
    }
    out
}

/// A task in `chores.yml`, structured just far enough to answer the
/// same question [`Step`] answers: does running it gate on its commands?
#[derive(Debug)]
struct ChoreTask {
    keys: Vec<String>,
    cmds: Vec<ChoreCmd>,
}

/// One item of a task's `cmds:`. Taskfile v3 spells it as a bare string,
/// or a mapping carrying `cmd:` (a shell command) or `task:` (another
/// task's name) beside keys such as `ignore_error:`.
#[derive(Debug)]
enum ChoreCmd {
    Shell {
        keys: Vec<String>,
        command: String,
    },
    Task {
        keys: Vec<String>,
        name: String,
    },
    /// `defer:` and anything else chore grows. Never counted: what the
    /// guard cannot read, it cannot say gates.
    Other,
}

/// Parse `chores.yml`'s `tasks:` into [`ChoreTask`]s, by name.
///
/// Panics on text it cannot parse, for the reason [`parse_workflow`]
/// does: a resolver that returned no tasks would turn every `chore`
/// line into a panic naming the wrong cause, and one that returned early
/// with a pass would be blind.
fn parse_chores(text: &str) -> std::collections::BTreeMap<String, ChoreTask> {
    let documents = Yaml::load_from_str(text).unwrap_or_else(|e| {
        panic!(
            "chores.yml is not valid YAML: {e}. The guard follows `chore <task>` into it, so \
             a file it cannot parse is a failure and never a pass."
        )
    });
    let mut tasks = std::collections::BTreeMap::new();
    let Some(mapping) = documents
        .first()
        .and_then(|document| field(document, "tasks"))
        .and_then(Yaml::as_mapping)
    else {
        return tasks;
    };
    for (name, body) in mapping.iter() {
        let Some(name) = name.as_str() else { continue };
        let cmds = field(body, "cmds")
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
            .map(|item| {
                if let Some(command) = item.as_str() {
                    return ChoreCmd::Shell {
                        keys: Vec::new(),
                        command: command.to_string(),
                    };
                }
                let keys = keys_of(item);
                if let Some(name) = field(item, "task").and_then(Yaml::as_str) {
                    ChoreCmd::Task {
                        keys,
                        name: name.to_string(),
                    }
                } else if let Some(command) = field(item, "cmd").and_then(Yaml::as_str) {
                    ChoreCmd::Shell {
                        keys,
                        command: command.to_string(),
                    }
                } else {
                    ChoreCmd::Other
                }
            })
            .collect();
        tasks.insert(
            name.to_string(),
            ChoreTask {
                keys: keys_of(body),
                cmds,
            },
        );
    }
    tasks
}

/// Keys whose presence on a chore TASK means running it may not run its
/// commands, or may not fail when they do.
///
/// `sources:`, `generates:` and `status:` are how a task tells chore it
/// is up to date, and an up-to-date task is SKIPPED: its commands do not
/// run and the step is green. That is exactly `if:` one file away. The
/// rest are the task-level spellings of `if:` and `continue-on-error:`
/// themselves. Presence, not value, as with [`NON_GATING_KEYS`], and for
/// the same reason: a guard that evaluated a fingerprint or a platform
/// list would acquire a new defeat whenever chore's syntax grows.
const NON_GATING_TASK_KEYS: [&str; 6] = [
    "sources",
    "generates",
    "status",
    "ignore_error",
    "platforms",
    "if",
];

/// Keys whose presence on one `cmds:` item means its failure is not the
/// task's failure, or that it may not run.
const NON_GATING_CMD_KEYS: [&str; 3] = ["ignore_error", "platforms", "if"];

fn carries_any(keys: &[String], forbidden: &[&str]) -> bool {
    keys.iter().any(|k| forbidden.contains(&k.as_str()))
}

/// The task names a step's `run:` hands to chore, in order.
///
/// A command -- as [`shell_commands`] splits a line -- whose first word
/// is `chore` (after any `NAME=value` assignments) names its task in the
/// first word after it that is not a flag, so `chore test:unit --force`
/// is `test:unit`. Comment lines and trailing comments are not commands,
/// as in [`checking_debug_runs`].
///
/// `! chore x` is NOT an invocation of `x` for this purpose: the `!`
/// inverts it, so the step is green exactly when the task fails.
fn chore_invocations(script: &str) -> Vec<String> {
    let mut found = Vec::new();
    for raw in script.lines() {
        let line = raw.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let line = line.split(" #").next().unwrap_or(line);
        for words in shell_commands(line) {
            let mut words = words
                .iter()
                .map(String::as_str)
                .skip_while(|w| w.contains('=') && !w.starts_with('-'));
            let Some(program) = words.next() else {
                continue;
            };
            if program != "chore" && !program.ends_with("/chore") {
                continue;
            }
            if let Some(task) = words.find(|w| !w.starts_with('-')) {
                found.push(task.to_string());
            }
        }
    }
    found
}

/// The checking debug runs chore performs, and cannot skip or discard,
/// when asked to run `task` -- following `task:` items down.
///
/// A task gates on its commands only if neither it nor any task on the
/// path to it carries a key from [`NON_GATING_TASK_KEYS`], and a command
/// counts only if its own item carries no key from
/// [`NON_GATING_CMD_KEYS`]. Each command is then held to
/// [`checking_debug_runs`], exactly as a command in a workflow step is.
///
/// A task naming itself, directly or through others, does not loop: the
/// repeated edge contributes nothing, which can only under-count.
///
/// # A TASK THAT DOES NOT EXIST IS A PANIC
///
/// Not an empty result. `chore test:unti` fails the CI step, so the
/// guard would report "no debug run gates" and send the reader looking
/// for a missing handshake; naming the task that is not there names the
/// real cause.
fn chore_checking_debug_runs(
    tasks: &std::collections::BTreeMap<String, ChoreTask>,
    task: &str,
    path: &mut Vec<String>,
) -> Vec<String> {
    if path.iter().any(|on_path| on_path == task) {
        return Vec::new();
    }
    let Some(body) = tasks.get(task) else {
        let mut via = path.join(" -> ");
        if via.is_empty() {
            via.push_str("a workflow step");
        }
        panic!(
            "`chore {task}` (from {via}) names no task in chores.yml. Its tasks: {:?}",
            tasks.keys().collect::<Vec<_>>()
        );
    };
    if carries_any(&body.keys, &NON_GATING_TASK_KEYS) {
        return Vec::new();
    }
    path.push(task.to_string());
    let mut out = Vec::new();
    for cmd in &body.cmds {
        match cmd {
            ChoreCmd::Shell { keys, command } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.extend(
                    checking_debug_runs(command)
                        .into_iter()
                        .map(|run| format!("chore {}: {run}", path.join(" -> "))),
                );
            }
            ChoreCmd::Task { keys, name } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.extend(chore_checking_debug_runs(tasks, name, path));
            }
            _ => {}
        }
    }
    path.pop();
    out
}

/// The checking debug runs of steps that ACTUALLY GATE a pull request,
/// following each `chore <task>` into `chores`, the text of chores.yml.
///
/// This is the function the guard below asks, and the whole of the
/// difference: [`checking_debug_runs`] finds the command, this asks
/// whether anything reads its result. Adding `if: false` to the
/// guarded step in `ci.yml`, or `continue-on-error: true`, left all
/// the guard's tests green while the gate stopped gating. See #142.
///
/// The chores text is a PARAMETER, not read here, so the rules can be
/// proved against small fixtures; the real guard hands it the
/// repository's own `chores.yml`.
fn gating_checking_debug_runs_via_chore(workflow: &str, chores: &str) -> Vec<String> {
    let tasks = parse_chores(chores);
    scan_steps(workflow, true, &|run| {
        let mut found = checking_debug_runs(run);
        for task in chore_invocations(run) {
            found.extend(chore_checking_debug_runs(&tasks, &task, &mut Vec::new()));
        }
        found
    })
}

/// [`gating_checking_debug_runs_via_chore`] with no chore tasks at all,
/// for the workflow-only fixtures in `mod gating`. A `chore` line in the
/// workflow panics here, as naming a task that does not exist.
fn gating_checking_debug_runs(workflow: &str) -> Vec<String> {
    gating_checking_debug_runs_via_chore(workflow, "tasks: {}\n")
}

fn workflow_path(name: &str) -> PathBuf {
    manifest_dir().join(".github").join("workflows").join(name)
}

/// The guard. Reads `ci.yml` -- the workflow that gates a pull request
/// -- and refuses if nothing there compiles the overflow checks and
/// asks the build to prove it, whether a step spells the run itself or
/// runs a `chores.yml` task that does.
///
/// `ci.yml` specifically, not `release.yml`. `release.yml` runs the
/// same debug task through `chore test`; it does not run on a pull
/// request, so its presence says nothing about whether a merge was
/// gated by it.
#[test]
fn the_pr_gate_still_tests_in_a_profile_that_can_see_an_overflow() {
    let path = workflow_path("ci.yml");
    let workflow = read_or_panic(&path);
    let chores = read_or_panic(&manifest_dir().join("chores.yml"));

    if let Some(why) = not_a_pull_request_gate(&workflow) {
        panic!("{}: {why}", path.display());
    }
    let debug_runs = gating_checking_debug_runs_via_chore(&workflow, &chores);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` reached from {} -- in a step, or in the chores.yml task a \
         `chore <task>` step runs -- runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1 IN A STEP WHOSE RESULT GATES A PULL REQUEST, so a \
         defect whose only symptom is an arithmetic overflow panic can merge without \
         the PR gate ever seeing it. The run belongs in chores.yml's `test:unit`, run \
         by ci.yml's `unit` job; a task carrying sources/generates/status (chore may \
         skip it) or a cmd carrying ignore_error does not count. release.yml running \
         it does not help: it triggers on a version tag, after the change has merged.",
        path.display()
    );
}

/// The parsed first document of `text`, read from `path`, or a panic
/// naming the file.
fn load_document<'t>(text: &'t str, path: &Path) -> Yaml<'t> {
    Yaml::load_from_str(text)
        .unwrap_or_else(|e| panic!("{} is not valid YAML: {e}", path.display()))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{} is empty", path.display()))
}

fn job<'a, 't>(document: &'a Yaml<'t>, name: &str, path: &Path) -> &'a Yaml<'t> {
    field(document, "jobs")
        .and_then(|jobs| field(jobs, name))
        .unwrap_or_else(|| panic!("{} has no jobs.{name}", path.display()))
}

fn steps_of<'a, 't>(job: &'a Yaml<'t>, name: &str) -> &'a [Yaml<'t>] {
    field(job, "steps")
        .and_then(Yaml::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("jobs.{name} has no steps"))
}

fn run_of<'a>(step: &'a Yaml) -> &'a str {
    field(step, "run").and_then(Yaml::as_str).unwrap_or("")
}

/// A job's `needs:`, which GitHub accepts as one name or a list of them.
fn needs_of(job: &Yaml) -> Vec<String> {
    match field(job, "needs") {
        Some(needs) if needs.as_sequence().is_some() => needs
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        Some(needs) => needs.as_str().map(str::to_string).into_iter().collect(),
        None => Vec::new(),
    }
}

/// Whether `step` is an `actions/<action>` step whose `with.name` is `artifact`.
fn is_artifact_step(step: &Yaml, action: &str, artifact: &str) -> bool {
    field(step, "uses")
        .and_then(Yaml::as_str)
        .is_some_and(|uses| uses.starts_with(&format!("actions/{action}@")))
        && field(step, "with")
            .and_then(|with| field(with, "name"))
            .and_then(Yaml::as_str)
            == Some(artifact)
}

/// Every step of `job` carries no non-gating key, and neither does the
/// job: the structure below is only the gate if all of it runs and all
/// of its failures count.
fn assert_unconditional(job: &Yaml, name: &str, workflow: &str) {
    assert!(
        !carries_a_non_gating_key(&keys_of(job)),
        "{workflow} jobs.{name} must not be conditional or allowed to fail"
    );
    for (at, step) in steps_of(job, name).iter().enumerate() {
        let keys = keys_of(step);
        // A STEP THAT RUNS NO COMMAND CANNOT GATE, AND MAY BE
        // CONDITIONAL (#264).
        //
        // The rule is right about `run:` steps: a conditional command is
        // a command that may not run, and the gate is the commands. It
        // was wrong about the others. `actions/upload-artifact` has no
        // `run:` — it cannot pass, cannot mask a failure and cannot make
        // a red job green — and without `if: always()` GitHub skips it
        // whenever an earlier step failed, which is the only time it is
        // worth having. The blanket ban therefore made the quiet suite's
        // logs unkeepable on exactly the runs that need them.
        //
        // `continue-on-error:` is still refused everywhere: on a `uses:`
        // step it says an upload that failed does not matter, which is a
        // different claim and not one this workflow makes.
        let commands = !run_of(step).is_empty();
        let offending: Vec<&str> = keys
            .iter()
            .map(String::as_str)
            .filter(|k| match *k {
                "if" => commands,
                "continue-on-error" => true,
                _ => false,
            })
            .collect();
        assert!(
            offending.is_empty(),
            "{workflow} jobs.{name} step {at} ({:?}) carries {offending:?}: a step that \
             runs a command must not be conditional or allowed to fail",
            field(step, "name")
                .and_then(Yaml::as_str)
                .unwrap_or(run_of(step))
        );
    }
}

/// Whether some step of `steps` runs `chore <task>`.
fn runs_chore(steps: &[Yaml], task: &str) -> bool {
    steps
        .iter()
        .any(|step| chore_invocations(run_of(step)).iter().any(|t| t == task))
}

/// THE PULL-REQUEST GATE'S SHAPE, now that every job runs chore tasks.
///
/// - `fixtures` builds the kernel-made images ONCE, in the
///   fs-linux-test-harness VM under KVM, on an x86_64 runner, and uploads
///   them. They are disk images, the same for every architecture.
/// - `test` runs on BOTH native architectures -- aarch64 matters on its
///   own: `c_char` is unsigned there, and Apple Silicon hosts link it
///   -- and downloads those images rather than building them, because
///   GitHub's arm64 runners have no KVM. So no step of it may start a VM.
/// - `unit` runs the tier that needs no tool and no fixture, which is
///   where the overflow handshake lives (the guard above).
/// - `ci-ok` is the one required check. It runs `if: always()` and must
///   need EVERY other job: a job it does not wait on can fail, or be
///   skipped, under a green required check. The set is computed from the
///   parsed jobs, so adding a job and forgetting ci-ok fails here.
///
/// And the tasks those jobs run must still be what they say:
/// `chore test` is unit, the tool and fixture checks, the whole suite in
/// release, and the script tests; `chore lint` is clippy with warnings
/// denied.
/// A pull request gets CI whatever it is based on (#132).
#[test]
fn ci_runs_on_a_pull_request_against_any_base() {
    let path = workflow_path("ci.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);
    let filter = pull_request_branch_filter(&document);
    assert!(
        filter.is_empty(),
        "ci.yml runs on pull requests only against {filter:?}. A pull request based on \
         anything else — a stacked one, which this repository uses routinely — runs no \
         CI at all, and still collects its review-bot ticks, so it reads as verified."
    );
}

#[test]
fn the_pr_gate_builds_fixtures_once_in_the_harness_vm_and_tests_both_architectures_through_chore() {
    let path = workflow_path("ci.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);

    // jobs.test — x86_64, where GitHub gives us KVM, so the oracles and
    // the kernel tests can run at all.
    let test = job(&document, "test", &path);
    assert_unconditional(test, "test", "ci.yml");
    let runner = field(test, "runs-on").and_then(Yaml::as_str).unwrap_or("");
    assert!(
        runner.starts_with("ubuntu-") && !runner.contains("arm"),
        "jobs.test must run on an x86_64 ubuntu runner: the oracle tools and the kernel \
         oracles run in a VM, and only those runners have KVM; got {runner:?}"
    );
    assert!(
        needs_of(test).iter().any(|n| n == "fixtures"),
        "jobs.test must need jobs.fixtures, whose artifact it tests against"
    );
    let steps = steps_of(test, "test");
    for task in ["tools", "lint", "test"] {
        assert!(
            runs_chore(steps, task),
            "jobs.test does not run `chore {task}`"
        );
    }
    assert!(
        steps
            .iter()
            .any(|step| run_of(step).contains("ci-setup-linux.sh")),
        "jobs.test must set the VM host up with the harness's ci-setup-linux.sh: every \
         oracle tool call and every kernel mount happens in that VM"
    );
    assert!(
        steps
            .iter()
            .any(|step| is_artifact_step(step, "download-artifact", "fixtures")),
        "jobs.test must download the `fixtures` artifact the fixtures job built"
    );
    // NOTHING INSTALLS THE ORACLE TOOLS ON THE RUNNER, and the job proves
    // it rather than claiming it.
    assert!(
        steps
            .iter()
            .all(|step| !run_of(step).contains("erofs-utils")),
        "jobs.test must not install erofs-utils on the runner: the tools live in the VM"
    );
    let checker = ["fsck", ".erofs"].concat();
    assert!(
        steps
            .iter()
            .any(|step| run_of(step).contains(&checker) && run_of(step).contains("::error::")),
        "jobs.test must make any oracle tool on the runner UNUSABLE before the tests \
         run, so that a green run is evidence every call went to the guest rather than \
         a claim that it did"
    );

    // jobs.test-arm64 — the architecture the driver ships on, on a runner
    // with no KVM: the tiers that need no VM.
    let arm = job(&document, "test-arm64", &path);
    assert_unconditional(arm, "test-arm64", "ci.yml");
    assert_eq!(
        field(arm, "runs-on").and_then(Yaml::as_str),
        Some("ubuntu-24.04-arm"),
        "jobs.test-arm64 must run on GitHub's arm64 runner"
    );
    let arm_steps = steps_of(arm, "test-arm64");
    for task in ["lint", "test:unit", "test:images"] {
        assert!(
            runs_chore(arm_steps, task),
            "jobs.test-arm64 does not run `chore {task}`"
        );
    }
    for step in arm_steps {
        let run = run_of(step);
        assert!(
            !run.contains("qemu-system-") && !run.contains("ci-setup-linux.sh"),
            "GitHub's arm64 runners have no KVM, so jobs.test-arm64 must not start a VM: \
             {run:?}"
        );
    }

    // jobs.suite-in-vm — the whole suite built and run INSIDE the guest,
    // which is how a host that is not Linux runs it.
    let in_vm = job(&document, "suite-in-vm", &path);
    assert_unconditional(in_vm, "suite-in-vm", "ci.yml");
    let in_vm_steps = steps_of(in_vm, "suite-in-vm");
    assert!(
        runs_chore(in_vm_steps, "test:vm"),
        "jobs.suite-in-vm must run `chore test:vm`, or the in-guest path rots unnoticed"
    );
    assert!(
        in_vm_steps
            .iter()
            .any(|step| run_of(step).contains("ci-setup-linux.sh")),
        "jobs.suite-in-vm must set the VM host up with the harness's ci-setup-linux.sh"
    );

    // jobs.fixtures
    let fixtures = job(&document, "fixtures", &path);
    assert_unconditional(fixtures, "fixtures", "ci.yml");
    let runner = field(fixtures, "runs-on")
        .and_then(Yaml::as_str)
        .unwrap_or("");
    assert!(
        runner.starts_with("ubuntu-") && !runner.contains("arm"),
        "jobs.fixtures must run on an x86_64 ubuntu runner, where KVM is available; got \
         {runner:?}"
    );
    let steps = steps_of(fixtures, "fixtures");
    let setup = steps
        .iter()
        .position(|step| run_of(step).contains("ci-setup-linux.sh"))
        .unwrap_or_else(|| {
            panic!("jobs.fixtures must set the VM host up with the harness's ci-setup-linux.sh")
        });
    let build = steps
        .iter()
        .position(|step| {
            chore_invocations(run_of(step))
                .iter()
                .any(|t| t == "fixtures")
        })
        .unwrap_or_else(|| panic!("jobs.fixtures must run `chore fixtures`"));
    assert!(
        setup < build,
        "jobs.fixtures must set up the VM host before `chore fixtures` needs it"
    );
    assert!(
        steps
            .iter()
            .any(|step| is_artifact_step(step, "upload-artifact", "fixtures")),
        "jobs.fixtures must upload the images as the `fixtures` artifact"
    );

    // jobs.unit
    let unit = job(&document, "unit", &path);
    assert_unconditional(unit, "unit", "ci.yml");
    assert!(
        runs_chore(steps_of(unit, "unit"), "test:unit"),
        "jobs.unit must run `chore test:unit`"
    );

    // jobs.ci-ok
    let ci_ok = job(&document, "ci-ok", &path);
    assert_eq!(
        field(ci_ok, "if").and_then(Yaml::as_str),
        Some("always()"),
        "jobs.ci-ok must run `if: always()`, or a failed job skips it and a skipped \
         required check can read as passing"
    );
    let mut others: Vec<String> = field(&document, "jobs")
        .map(keys_of)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name != "ci-ok")
        .collect();
    let mut needed = needs_of(ci_ok);
    others.sort();
    needed.sort();
    assert_eq!(
        needed, others,
        "jobs.ci-ok must need exactly every other job in ci.yml: a job it does not wait \
         on can fail under a green required check"
    );

    // chores.yml
    let chores_path = manifest_dir().join("chores.yml");
    let tasks = parse_chores(&read_or_panic(&chores_path));
    let task = |name: &str| {
        tasks
            .get(name)
            .unwrap_or_else(|| panic!("chores.yml has no `{name}` task"))
    };
    // `test` chooses between the native run and the in-guest one; the
    // native side is what the gate on a Linux runner executes.
    let dispatcher = task("test");
    assert!(
        !carries_any(&dispatcher.keys, &NON_GATING_TASK_KEYS),
        "chores.yml `test` must never be skipped as up to date or allowed to fail"
    );
    assert!(
        dispatcher.cmds.iter().any(|cmd| matches!(
            cmd,
            ChoreCmd::Shell { command, .. } if command.contains("test:vm")
        )),
        "chores.yml `test` must fall to `test:vm` on a host that is not Linux, so the \
         Linux tests always run on Linux"
    );
    let test_task = task("test:native");
    assert!(
        !carries_any(&test_task.keys, &NON_GATING_TASK_KEYS),
        "chores.yml `test` must never be skipped as up to date or allowed to fail"
    );
    let commands: Vec<&str> = test_task
        .cmds
        .iter()
        .filter_map(|cmd| match cmd {
            ChoreCmd::Shell { keys, command } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                Some(command.as_str())
            }
            _ => None,
        })
        .collect();
    let subtasks: Vec<&str> = test_task
        .cmds
        .iter()
        .filter_map(|cmd| match cmd {
            ChoreCmd::Task { keys, name } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                Some(name.as_str())
            }
            _ => None,
        })
        .collect();
    for subtask in [
        "test:unit",
        "test:images",
        "test:oracle",
        "test:kernel",
        "test:scripts",
    ] {
        assert!(
            subtasks.contains(&subtask),
            "chores.yml `test:native` must run `task: {subtask}`; it runs {subtasks:?}"
        );
    }
    // Spelled in two pieces so this file does not name the fixture
    // directory: scripts/test-targets.sh would otherwise count it as a
    // test that needs fixtures and leave it out of `chore test:unit`.
    let fixtures_check = ["test-", "disks/build-fixtures.sh --check"].concat();
    for check in ["scripts/tools.sh --check", fixtures_check.as_str()] {
        assert!(
            commands.iter().any(|c| c.trim() == check),
            "chores.yml `test:native` must run `{check}` first, so a missing tool or fixture fails \
             once, naming the task that provides it; it runs {commands:?}"
        );
    }
    // THE WHOLE SUITE, AND `all` IS WHAT MAKES IT WHOLE. A bare
    // `cargo test` would also select the Android suite, whose downloaded
    // fixture no machine here can be expected to have, so the selection is
    // `scripts/test-targets.sh all` -- every target the tiers cover, in
    // one release run. Any OTHER test-targets.sh selection is a tier
    // repeated, not the suite, and does not satisfy this.
    assert!(
        commands.iter().any(|c| {
            let as_cargo = c.replace(TEST_WRAPPER, "cargo test");
            as_cargo.contains("cargo test")
                && as_cargo.contains("--release")
                && as_cargo.contains("test-targets.sh all")
        }),
        "chores.yml `test:native` must run the whole suite in the release profile, \
         selected with `scripts/test-targets.sh all`; it runs {commands:?}"
    );
    assert!(
        task("lint").cmds.iter().any(|cmd| matches!(
            cmd,
            ChoreCmd::Shell { keys, command }
                if !carries_any(keys, &NON_GATING_CMD_KEYS)
                    && command.trim()
                        == "cargo clippy --locked --all-targets --features cli -- -D warnings"
        )),
        "chores.yml `lint` must run `cargo clippy --locked --all-targets --features cli -- -D \
         warnings`: without the feature the command-line tools are not linted at all"
    );
}

// --- THE CROSS-VALIDATION GATE (#124) --------------------------------
//
// The only check here that is not this crate marking its own homework is
// the one where an independent tool reads what the writer wrote. It used
// to be a job of its own, `validate mkfs_erofs (fsck.erofs strict)`; the
// harness migration (#134) folded it into the `oracle` tier, which `jobs.
// test` reaches through `chore test`. Nothing asserted that it still does.
// A job given an `if:`, a step given `continue-on-error:`, a tier command
// given `ignore_error:` or a `--skip`, a harness that stopped provisioning
// the tool: each leaves a green check whose name still reads like
// cross-validation while nothing is cross-validated. An executed-test
// floor cannot see that when the tier still runs its other tests, and a
// required check cannot see it at all.
//
// So the three facts are asserted separately, because they fail
// separately: the path GATES a merge, the job PROVIDES the tool, and what
// runs HANDS THE WRITER'S OUTPUT TO fsck.erofs.

/// The chore tasks one `run:` or `cmd:` hands to chore: `chore <task>`,
/// and the `"{{.CHORE_EXE}}" <task>` spelling one task uses to run
/// another, which is how `test` reaches `test:native`.
fn chore_tasks_in(script: &str) -> Vec<String> {
    let mut found = chore_invocations(script);
    for line in script.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let Some((_, rest)) = line.split_once("{{.CHORE_EXE}}") else {
            continue;
        };
        let rest = rest.trim_start_matches(['"', '\'']);
        if let Some(task) = rest.split_whitespace().find(|w| !w.starts_with('-')) {
            found.push(task.trim_matches(['"', '\'']).to_string());
        }
    }
    found
}

/// Every shell command chore runs, and can neither skip nor discard, when
/// asked for `task` -- following `task:` items and nested chore calls.
/// A task that does not exist contributes nothing: `chore` fails on it,
/// so nothing runs, and the guards below then report the path missing.
fn gating_chore_commands(
    tasks: &std::collections::BTreeMap<String, ChoreTask>,
    task: &str,
    path: &mut Vec<String>,
) -> Vec<String> {
    if path.iter().any(|on_path| on_path == task) {
        return Vec::new();
    }
    let Some(body) = tasks.get(task) else {
        return Vec::new();
    };
    if carries_any(&body.keys, &NON_GATING_TASK_KEYS) {
        return Vec::new();
    }
    path.push(task.to_string());
    let mut out = Vec::new();
    for cmd in &body.cmds {
        match cmd {
            ChoreCmd::Shell { keys, command } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.push(command.clone());
                for next in chore_tasks_in(command) {
                    out.extend(gating_chore_commands(tasks, &next, path));
                }
            }
            ChoreCmd::Task { keys, name } if !carries_any(keys, &NON_GATING_CMD_KEYS) => {
                out.extend(gating_chore_commands(tasks, name, path));
            }
            _ => {}
        }
    }
    path.pop();
    out
}

/// The `scripts/test-targets.sh` selection a command runs the suite
/// over, if it runs one in full. A `--skip` narrows the run to less than
/// the selection, so a command carrying one does not count: `--skip
/// fsck` would keep every file selected and every writer check gone.
fn full_suite_selection(command: &str) -> Option<String> {
    if !(command.contains("scripts/test.sh") || command.contains("cargo test")) {
        return None;
    }
    if command.contains("--skip") {
        return None;
    }
    let (_, rest) = command.split_once("test-targets.sh ")?;
    let selection: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!selection.is_empty()).then_some(selection)
}

/// One way a pull request reaches the writer's cross-validation: the job,
/// the index of the step, and the command at the end of the path.
#[derive(Debug)]
struct CrossValidationPath {
    job: String,
    step: usize,
    command: String,
}

/// Every path by which `workflow`, on a pull request, runs a selection
/// that `selects_writer` says holds the writer's cross-validation --
/// through jobs and steps that carry no `if:` or `continue-on-error:`,
/// and chore tasks and commands that carry no key letting them be skipped
/// or fail quietly.
fn cross_validation_paths(
    workflow: &str,
    chores: &str,
    selects_writer: &dyn Fn(&str) -> bool,
) -> Vec<CrossValidationPath> {
    if !runs_on_pull_request(&parse_workflow(workflow)) {
        return Vec::new();
    }
    let tasks = parse_chores(chores);
    let documents = Yaml::load_from_str(workflow).expect("workflow is valid YAML");
    let Some(jobs) = documents
        .first()
        .and_then(|document| field(document, "jobs"))
        .and_then(Yaml::as_mapping)
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (name, body) in jobs.iter() {
        let Some(name) = name.as_str() else { continue };
        if carries_a_non_gating_key(&keys_of(body)) {
            continue;
        }
        let steps = field(body, "steps")
            .and_then(Yaml::as_sequence)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (at, step) in steps.iter().enumerate() {
            if carries_a_non_gating_key(&keys_of(step)) {
                continue;
            }
            for task in chore_invocations(run_of(step)) {
                for command in gating_chore_commands(&tasks, &task, &mut Vec::new()) {
                    if full_suite_selection(&command).is_some_and(|s| selects_writer(&s)) {
                        paths.push(CrossValidationPath {
                            job: name.to_string(),
                            step: at,
                            command,
                        });
                    }
                }
            }
        }
    }
    paths
}

/// The test file whose tests build an image with this crate's writer and
/// hand it to `fsck.erofs`.
const WRITER_CROSS_VALIDATION: &str = "oracle_writer";

/// Whether `scripts/test-targets.sh <selection>` selects
/// [`WRITER_CROSS_VALIDATION`], asked of the real script: the selection
/// is derived from the tests by grep, so only running it answers.
fn real_selection_holds_writer(selection: &str) -> bool {
    let out = std::process::Command::new("bash")
        .arg(manifest_dir().join("scripts/test-targets.sh"))
        .arg(selection)
        .output()
        .unwrap_or_else(|e| panic!("cannot run scripts/test-targets.sh {selection}: {e}"));
    assert!(
        out.status.success(),
        "scripts/test-targets.sh {selection} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == WRITER_CROSS_VALIDATION)
}

fn real_cross_validation_paths() -> Vec<CrossValidationPath> {
    let workflow = read_or_panic(&workflow_path("ci.yml"));
    let chores = read_or_panic(&manifest_dir().join("chores.yml"));
    cross_validation_paths(&workflow, &chores, &real_selection_holds_writer)
}

/// FACT ONE: what runs hands the writer's output to `fsck.erofs`.
///
/// The pull-request gate reaches, unconditionally, a full run of a
/// selection holding `tests/oracle_writer.rs`; that file still builds
/// images with `mkfs::build_image` and runs `fsck.erofs` over them; and
/// the `oracle` tier, the one that exists to run it, still has a floor.
#[test]
fn the_pr_gate_still_hands_the_writers_images_to_fsck_erofs() {
    let paths = real_cross_validation_paths();
    assert!(
        !paths.is_empty(),
        "no step of ci.yml that gates a pull request reaches a full run of a \
         scripts/test-targets.sh selection holding tests/{WRITER_CROSS_VALIDATION}.rs. \
         Something on the path from a job to `chore test:oracle` became conditional \
         (`if:`), allowed to fail (`continue-on-error:`, `ignore_error:`), skippable as \
         up to date (`sources:`), or narrowed with `--skip`; or the selection stopped \
         holding that file."
    );

    let writer = read_or_panic(
        &manifest_dir()
            .join("tests")
            .join(format!("{WRITER_CROSS_VALIDATION}.rs")),
    );
    let invokes_fsck = ["oracle", "(\"fsck.erofs\")"].concat();
    assert!(
        writer.contains("mkfs::build_image(") && writer.contains(&invokes_fsck),
        "tests/{WRITER_CROSS_VALIDATION}.rs must build images with this crate's writer \
         (mkfs::build_image) and run fsck.erofs over them through the oracle helper"
    );
    assert!(
        writer.matches("#[test]").count() >= 5,
        "tests/{WRITER_CROSS_VALIDATION}.rs has almost no tests left"
    );

    let chores = parse_chores(&read_or_panic(&manifest_dir().join("chores.yml")));
    let floor = gating_chore_commands(&chores, "test:oracle", &mut Vec::new())
        .iter()
        .find_map(|c| {
            c.trim()
                .strip_prefix("bash scripts/core.sh test-floor --refuse-ignored oracle ")
                .and_then(|n| n.trim().parse::<u32>().ok())
        });
    assert!(
        floor.is_some_and(|n| n > 0),
        "chores.yml `test:oracle` must end in `bash scripts/core.sh test-floor --refuse-ignored oracle N` \
         with N > 0, \
         so the tier cannot go green having run nothing; found {floor:?}"
    );
}

/// FACT TWO: the job that runs it gates the merge.
///
/// A path above is only a gate if its job is one the required check
/// waits for: `ci-ok` must need it and run `if: always()`, and
/// `.github-guard` must require `ci-ok`.
#[test]
fn the_job_that_cross_validates_against_fsck_erofs_gates_the_merge() {
    let paths = real_cross_validation_paths();
    let path = workflow_path("ci.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);
    let ci_ok = job(&document, "ci-ok", &path);
    let needed = needs_of(ci_ok);
    assert!(
        paths.iter().any(|p| needed.contains(&p.job)),
        "no job that cross-validates against fsck.erofs is in ci-ok's needs: \
         paths {:?}, ci-ok needs {needed:?}",
        paths
            .iter()
            .map(|p| format!("jobs.{} step {} runs {}", p.job, p.step, p.command))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        field(ci_ok, "if").and_then(Yaml::as_str),
        Some("always()"),
        "jobs.ci-ok must run `if: always()`: a failed cross-validation job would \
         otherwise skip it, and a skipped required check never reports"
    );
    let guard = read_or_panic(&manifest_dir().join(".github-guard"));
    let required: Vec<&str> = guard
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("required"))
        .filter_map(|l| l.trim_start().strip_prefix('='))
        .map(str::trim)
        .collect();
    assert_eq!(
        required,
        vec!["ci-ok"],
        ".github-guard must require ci-ok, the check that stands for the \
         cross-validation job"
    );
}

/// FACT THREE: the job provides the tool.
///
/// `fsck.erofs` lives in the fs-linux-test-harness VM and nowhere else.
/// So the job must set the VM host up before the step that runs the
/// tier; the harness must be told to provision the guest with
/// `scripts/vm-setup.sh`; and that script must build a `fsck.erofs`
/// that accepts what `mkfs.erofs` wrote, and say which version it is.
#[test]
fn the_job_that_cross_validates_against_fsck_erofs_provides_the_tool() {
    let paths = real_cross_validation_paths();
    let path = workflow_path("ci.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);
    let provided = paths.iter().any(|p| {
        let steps = steps_of(job(&document, &p.job, &path), &p.job);
        steps[..p.step].iter().any(|step| {
            !carries_a_non_gating_key(&keys_of(step)) && run_of(step).contains("ci-setup-linux.sh")
        })
    });
    assert!(
        provided,
        "no job that cross-validates against fsck.erofs sets up the VM host \
         (the harness's ci-setup-linux.sh) before the step that runs the tier: \
         {paths:?}"
    );

    let harness = read_or_panic(&manifest_dir().join("fs-linux-test-harness.toml"));
    let mut section = "";
    let mut setup = None;
    for line in harness.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if section == "[setup]" {
            if let Some(v) = line.strip_prefix("script") {
                setup = v
                    .trim_start()
                    .strip_prefix('=')
                    .map(|v| v.trim().trim_matches('"'));
            }
        }
    }
    assert_eq!(
        setup,
        Some("scripts/vm-setup.sh"),
        "fs-linux-test-harness.toml [setup] script must be scripts/vm-setup.sh, which \
         builds the oracle tools in the guest"
    );
    let vm_setup = read_or_panic(&manifest_dir().join("scripts/vm-setup.sh"));
    let checker = ["fsck", ".erofs"].concat();
    assert!(
        vm_setup.contains(&format!("{checker} \"$probe/out.img\""))
            && vm_setup.contains(&format!("{checker} --version")),
        "scripts/vm-setup.sh must run {checker} over an image mkfs.erofs just wrote, \
         and print its version, so a guest without a working checker fails provisioning"
    );
}

mod cross_validation_paths_rules {
    use super::cross_validation_paths;

    const CHORES: &str = "version: '3'\ntasks:\n  test:\n    cmds:\n      - task: test:oracle\n  \
                          test:oracle:\n    cmds:\n      - 'scripts/tier.sh test:oracle oracle 1 1 -- \
                          scripts/test.sh --locked --release $(scripts/test-targets.sh oracle)'\n      \
                          - 'bash scripts/core.sh test-floor --refuse-ignored oracle 80'\n";

    fn workflow(job_keys: &str, step_keys: &str) -> String {
        format!(
            "on:\n  pull_request:\njobs:\n  test:\n{job_keys}    runs-on: ubuntu-24.04\n    \
             steps:\n      - run: chore test\n{step_keys}"
        )
    }

    fn found(workflow: &str, chores: &str) -> usize {
        cross_validation_paths(workflow, chores, &|s| s == "oracle").len()
    }

    #[test]
    fn the_plain_path_is_found() {
        assert_eq!(found(&workflow("", ""), CHORES), 1);
    }

    #[test]
    fn a_conditional_job_is_not_a_gate() {
        assert_eq!(found(&workflow("    if: false\n", ""), CHORES), 0);
    }

    #[test]
    fn a_step_allowed_to_fail_is_not_a_gate() {
        assert_eq!(
            found(&workflow("", "        continue-on-error: true\n"), CHORES),
            0
        );
    }

    #[test]
    fn a_workflow_that_does_not_run_on_pull_requests_is_not_a_gate() {
        let wf = workflow("", "").replace("pull_request:", "push:");
        assert_eq!(found(&wf, CHORES), 0);
    }

    #[test]
    fn an_ignored_tier_command_is_not_a_gate() {
        let chores = CHORES.replace(
            "      - 'scripts/tier.sh",
            "      - ignore_error: true\n        cmd: 'scripts/tier.sh",
        );
        assert_eq!(found(&workflow("", ""), &chores), 0);
    }

    #[test]
    fn a_task_that_can_be_up_to_date_is_not_a_gate() {
        let chores = CHORES.replace(
            "  test:oracle:\n",
            "  test:oracle:\n    sources: ['Cargo.toml']\n",
        );
        assert_eq!(found(&workflow("", ""), &chores), 0);
    }

    #[test]
    fn a_run_narrowed_with_skip_is_not_a_gate() {
        let chores = CHORES.replace(
            "test-targets.sh oracle)'",
            "test-targets.sh oracle) -- --skip fsck'",
        );
        assert_eq!(found(&workflow("", ""), &chores), 0);
    }

    #[test]
    fn a_selection_without_the_writer_is_not_a_gate() {
        let chores = CHORES.replace("test-targets.sh oracle)", "test-targets.sh images)");
        assert_eq!(found(&workflow("", ""), &chores), 0);
    }

    /// `test` reaches `test:native` through `"{{.CHORE_EXE}}"`, not a
    /// `task:` item, and the path must still be followed.
    #[test]
    fn a_nested_chore_call_is_followed() {
        let chores = CHORES.replace(
            "      - task: test:oracle\n",
            "      - |\n        \"{{.CHORE_EXE}}\" test:oracle\n",
        );
        assert_eq!(found(&workflow("", ""), &chores), 1);
    }
}

/// THE SHIPPING GATE runs the same chore tasks as the pull-request gate,
/// in one job: `ubuntu-latest` is x86_64 with KVM, so it builds the
/// fixtures itself through the harness VM -- `ci-setup-linux.sh`, then
/// `chore fixtures` -- and then runs `chore test`, unconditionally.
///
/// None of the scripts the harness replaced may come back: the native
/// sudo generator, the in-repo VM builder and e2fsck runner, and the
/// in-repo VM wrapper are deleted, and a step naming one fails on a tag,
/// after the version is committed to. And nothing ships -- neither the
/// packaged binary nor the crates.io upload -- unless `test` passed.
#[test]
fn the_release_gate_builds_fixtures_in_the_harness_vm_and_runs_chore_test() {
    let path = workflow_path("release.yml");
    let text = read_or_panic(&path);
    let document = load_document(&text, &path);
    let test = job(&document, "test", &path);
    assert_unconditional(test, "test", "release.yml");
    let steps = steps_of(test, "test");

    let setup = steps
        .iter()
        .position(|step| run_of(step).contains("ci-setup-linux.sh"))
        .unwrap_or_else(|| {
            panic!("release jobs.test must set the VM host up with the harness's ci-setup-linux.sh")
        });
    let build = steps
        .iter()
        .position(|step| {
            chore_invocations(run_of(step))
                .iter()
                .any(|t| t == "fixtures")
        })
        .unwrap_or_else(|| panic!("release jobs.test must run `chore fixtures`"));
    assert!(
        setup < build,
        "release jobs.test must set up the VM host before `chore fixtures` needs it"
    );
    assert!(
        runs_chore(steps, "test"),
        "release jobs.test must run `chore test`"
    );

    let jobs = field(&document, "jobs")
        .and_then(Yaml::as_mapping)
        .unwrap_or_else(|| panic!("{} has no jobs", path.display()));
    for (name, body) in jobs.iter() {
        for step in field(body, "steps")
            .and_then(Yaml::as_sequence)
            .into_iter()
            .flatten()
        {
            let run = run_of(step);
            // None of what the harness replaced may come back: the
            // 60-line erofs-utils source build that stood in this
            // workflow and in ci.yml, and the `--ignored` opt-in that
            // was how the oracles were selected before they became a
            // tier of their own.
            for deleted in [
                "erofs-utils-1.9.1",
                "--enable-fuse",
                "-- --ignored",
                "scripts/vm.sh",
            ] {
                assert!(
                    !run.contains(deleted),
                    "release.yml jobs.{} runs `{deleted}`, which is deleted: {run:?}",
                    name.as_str().unwrap_or("?")
                );
            }
        }
    }

    // Every shipping job -- the crates.io upload, and the call to core's
    // release-cli workflow that packages the command-line tarballs and
    // attaches them to the release -- follows one rule: nothing ships
    // unless `test` passed.
    for shipping in ["publish", "cli"] {
        assert!(
            needs_of(job(&document, shipping, &path))
                .iter()
                .any(|n| n == "test"),
            "release jobs.{shipping} must need jobs.test: nothing ships untested"
        );
    }
}

/// THE DISTINCTION THIS REPOSITORY NEEDS THAT A PORTED COPY WOULD MISS.
///
/// A workflow carrying a checking debug run under a name other than
/// `ci.yml` -- `release.yml`, in this repository's own case -- must not
/// satisfy the guard. `release.yml` does carry one: its `chore test`
/// runs `task: test:unit`, handshake and all, unconditionally. That is
/// exactly the near miss worth pinning, because it does not make
/// `ci.yml`'s own absence of one acceptable: `release.yml` triggers on a
/// version tag, too late to gate a merge.
#[test]
fn a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard() {
    let release_yml_shape = "\
jobs:
  release:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets
";
    // This function only ever reads ci.yml in the real guard above; this
    // test pins that the PARSER itself would still count such a line if
    // handed the wrong file, so the guard's safety is coming from WHICH
    // FILE it opens -- a fact worth being explicit about, since a future
    // edit that widened the scan to every workflow would silently stop
    // catching this repository's actual defect.
    assert_eq!(
        checking_debug_runs(release_yml_shape),
        vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets".to_string()],
        "the parser itself would count this line -- the guard's correctness \
         depends on scanning ci.yml and ci.yml alone, not on the parser \
         refusing this shape"
    );

    // And release.yml's actual shape: the same run, one `chore test` away,
    // in a workflow that triggers on a tag. The resolver finds the run --
    // so the chore indirection is not what refuses it -- and the gating
    // walk refuses it for the trigger alone.
    let release_yml_via_chore = "\
on:
  push:
    tags: ['v*.*.*']
jobs:
  test:
    steps:
      - run: chore test
";
    let chores = "\
tasks:
  test:
    cmds:
      - task: test:unit
      - scripts/test.sh --locked --release
  test:unit:
    cmds:
      - 'EXPECT_OVERFLOW_CHECKS=1 scripts/test.sh --locked --lib'
";
    let tasks = parse_chores(chores);
    assert_eq!(
        chore_checking_debug_runs(&tasks, "test", &mut Vec::new()).len(),
        1,
        "the resolver itself would find the run behind `chore test`"
    );
    assert!(
        gating_checking_debug_runs_via_chore(release_yml_via_chore, chores).is_empty(),
        "a workflow that runs on a tag gates no merge, however its task resolves"
    );
    assert_eq!(
        gating_checking_debug_runs_via_chore(
            &release_yml_via_chore.replace("  push:\n    tags: ['v*.*.*']\n", "  pull_request:\n"),
            chores
        )
        .len(),
        1,
        "control: the same steps on pull_request do gate"
    );
}

/// Whether the step's result is READ, which the line-based parser
/// above cannot see. Six conditions, each with its own test, plus a
/// control asserting the unmodified shape IS counted so the others
/// cannot pass for the wrong reason.
mod gating {
    use super::gating_checking_debug_runs as gating;

    /// The shape that does gate. Every test below is this with one
    /// thing changed, so a failure here means the fixture is wrong
    /// rather than the property.
    const GATING: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";

    const STEP: &str = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";

    /// THE MESSAGE NAMES THE CAUSE (#155). A workflow that stopped
    /// triggering on pull requests is reported as that, with the triggers
    /// it has, and not as a missing debug step. The control is the gating
    /// shape, which has nothing to explain.
    #[test]
    fn a_workflow_off_pull_requests_is_reported_by_its_trigger() {
        assert_eq!(super::not_a_pull_request_gate(GATING), None, "control");
        for (trigger, names_target) in [
            ("pull_request_target", true),
            ("pull_request_review", false),
            ("push", false),
        ] {
            let yaml = GATING.replace("  pull_request:\n", &format!("  {trigger}:\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            let why = super::not_a_pull_request_gate(&yaml)
                .unwrap_or_else(|| panic!("{trigger}: no reason given"));
            assert!(
                why.contains(&format!("{trigger:?}")) && why.contains("`on:` block"),
                "{trigger}: the message must name the trigger found and the on: block: {why}"
            );
            assert_eq!(
                why.contains("refused on purpose"),
                names_target,
                "{trigger}: only pull_request_target gets the refusal explained: {why}"
            );
        }
    }

    #[test]
    fn the_control_shape_gates() {
        assert_eq!(
            gating(GATING).len(),
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
            let yaml = GATING.replace(STEP, &format!("{STEP}        {condition}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a step carrying `{condition}` may or may not run, so it cannot be what \
                 makes the gate able to see an overflow. Rejected on the key's presence \
                 rather than by evaluating it -- the spellings are open-ended."
            );
        }
    }

    #[test]
    fn a_step_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace(STEP, &format!("{STEP}        continue-on-error: true\n"));
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "the step runs and its failure is discarded, which is this project's own \
             named defect: a step that runs and whose result nothing reads"
        );
    }

    #[test]
    fn a_job_carrying_if_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    if: false\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "the same reasoning one level up: a job that may not run cannot gate"
        );
    }

    #[test]
    fn a_job_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    continue-on-error: true\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "a job whose failure is discarded cannot gate, however sound its steps"
        );
    }

    /// The assumption the `ci.yml`-only scope rests on, which is a
    /// fact about the file rather than a given.
    #[test]
    fn a_workflow_that_no_longer_runs_on_pull_request_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  push:\n    branches: [main]\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "scoping the scan to ci.yml assumes ci.yml is what runs on a pull request; \
             if its triggers stop including pull_request, the step gates nothing no \
             matter how it looks"
        );
    }

    /// `pull_request_target` ALONE IS NOT A PULL-REQUEST GATE, and
    /// `runs_on_pull_request` does not count it: it compares each
    /// parsed trigger name against `pull_request` and nothing else.
    ///
    /// Such a workflow runs in the base repository's context and checks
    /// out the base ref by default, so it may never build the
    /// contributor's code (#149). Replacing `pull_request:` with
    /// `pull_request_target:` is a defeat of the gate, and this test is
    /// what kills that mutation.
    #[test]
    fn a_pull_request_target_trigger_alone_does_not_gate() {
        let yaml = GATING.replace("  pull_request:\n", "  pull_request_target:\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "pull_request_target runs against the BASE repository with a write token and \
             the repository's secrets, and checks out the base ref by default, so a \
             workflow triggered only that way may never build the contributor's code. \
             It is not proof that the merge is gated. See rust-fs-ext4#149."
        );
    }

    /// THE CONTROL THAT STOPS THE REFUSAL OVER-CORRECTING.
    ///
    /// Carrying both triggers is the ordinary way to reach secrets
    /// without giving up the gate, and such a workflow IS gated -- by
    /// its `pull_request:` key, which the refusal above must not
    /// disturb. Without this test, narrowing the comparison to
    /// `t == "pull_request" && !any(t == "pull_request_target")` would
    /// pass every other assertion in this file while refusing a
    /// perfectly gated workflow. Contributed by the branch this change
    /// supersedes; it is the arm that branch added and the reason to
    /// keep it whatever the parser looks like.
    #[test]
    fn a_workflow_carrying_both_triggers_still_gates() {
        let yaml = GATING.replace(
            "  pull_request:\n",
            "  pull_request:\n  pull_request_target:\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "the workflow still triggers on pull_request, so it still gates; refusing it \
             would be the over-correction"
        );
    }

    /// A `#` inside a quoted shell value is content, not a comment.
    ///
    /// This is what `without_comment` defended, and it is the reason
    /// that helper existed: a hand-rolled scanner has to decide where
    /// a comment starts, and its own doc conceded the rule was "crude
    /// next to real YAML". The parser decides it by the grammar --
    /// inside a quoted scalar a `#` is simply a character -- so the
    /// property is asserted here rather than left to a heuristic.
    #[test]
    fn a_hash_inside_a_quoted_value_is_content_not_a_comment() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: echo '#1'; EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "the `#` is inside a quoted shell string, so the command after it is still \
             the command; treating it as a comment would refuse a correct workflow"
        );
    }

    /// A workflow the parser cannot read is a failure, never a pass.
    ///
    /// The direction matters: swallowing the error and returning an
    /// empty structure would report "no debug run gates this", which is
    /// also a failure and therefore safe -- but returning early with a
    /// pass would be the blindness this module exists to refuse.
    #[test]
    #[should_panic(expected = "not valid YAML")]
    fn a_workflow_that_does_not_parse_is_a_failure() {
        super::parse_workflow("jobs:\n  test:\n   - broken: [unclosed\n");
    }

    /// THE OTHER DIRECTION, which is the one that costs something.
    ///
    /// A step's `env:` mapping says nothing about whether its result
    /// is read, so it must NOT disqualify the step. Over-strictness
    /// here would be self-defeating: the handshake this guard looks
    /// for is itself an environment variable, and a maintainer moving
    /// it into a mapping would turn the guard red on a workflow that
    /// gates perfectly well. Pinned so a later tightening of the
    /// non-gating key list cannot quietly swallow it.
    #[test]
    fn a_step_carrying_an_env_mapping_still_gates() {
        let yaml = GATING.replace(
            STEP,
            "      - env:\n          CARGO_TERM_COLOR: always\n        run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "an `env:` mapping is not a condition and does not discard a result, so the \
             step still gates. Rejecting it would be over-strict in the one direction \
             that breaks a working workflow."
        );
    }

    /// A TRAILING COMMENT ON THE JOB LINE MUST NOT LOSE THE JOB.
    ///
    /// The job header is recognised by its line ending in a colon, and
    /// `  test:  # the gate` does not -- so the job's steps were never
    /// collected and the guard reported that nothing gates. Loud
    /// rather than silent, but wrong, and on a workflow that gates
    /// perfectly well. The over-strict direction is the safe one for a
    /// CONDITION; it is not safe for a comment.
    #[test]
    fn a_job_line_with_a_trailing_comment_still_gates() {
        let yaml = GATING.replace("  test:\n", "  test:  # the pull-request gate\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "a comment after the job's name says nothing about whether its result is read"
        );
    }

    /// A COMMENT IS NOT A TRIGGER. The substring search this replaced
    /// answered `true` for the comment that says the trigger was
    /// switched off, which is the most likely place the word appears
    /// on a workflow that no longer gates.
    #[test]
    fn a_commented_out_pull_request_trigger_does_not_gate() {
        for spelling in [
            "  push:\n    branches: [main]\n  # pull_request disabled for now\n",
            "  push:\n    branches: [main]\n    # was: pull_request\n",
            "  push: # replaces pull_request\n    branches: [main]\n",
        ] {
            let yaml = GATING.replace("  pull_request:\n    branches: [main]\n", spelling);
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                yaml.contains("pull_request"),
                "precondition: the word must still be PRESENT, or this tests nothing -- \
                 the whole point is text that mentions it while not triggering on it"
            );
            assert!(
                gating(&yaml).is_empty(),
                "a workflow whose only mention of pull_request is a comment gates \
                 nothing. Spelling: {spelling:?}"
            );
        }
    }

    /// A DIFFERENT TRIGGER THAT STARTS THE SAME WAY IS A DIFFERENT
    /// TRIGGER. `pull_request_review` fires on review events, not on a
    /// pull request opening or being pushed to, so a step under it
    /// cannot be what gates the pull request.
    ///
    /// This is why the check compares whole trigger names rather than
    /// matching a prefix.
    #[test]
    fn a_similarly_named_trigger_does_not_gate() {
        for trigger in [
            "pull_request_review",
            "pull_request_review_comment",
            "pull_requests",
        ] {
            let yaml = GATING.replace("  pull_request:\n", &format!("  {trigger}:\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "`{trigger}` is not `pull_request`, so it must not be counted as one"
            );
        }
    }

    /// A QUOTED KEY IS THE SAME KEY. `"if": false` is valid YAML and
    /// GitHub Actions honours it exactly as `if: false`, but a raw
    /// text compare against `if` matched neither quoted spelling -- so
    /// a step that does not gate was counted as one that does.
    ///
    /// This file's manifest parser already normalises quotes, after a
    /// quoted `"overflow-checks" = false` defeated that scan. Same
    /// defect one format across.
    #[test]
    fn a_quoted_non_gating_key_still_does_not_gate() {
        for key in [
            "\"if\": false",
            "'if': false",
            "\"continue-on-error\": true",
            "'continue-on-error': true",
        ] {
            let yaml = GATING.replace(STEP, &format!("{STEP}        {key}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a step carrying `{key}` does not gate, and the quotes do not change that"
            );
        }
    }

    /// The same, one level up, where the job-level key extraction had
    /// the identical bypass.
    #[test]
    fn a_quoted_non_gating_key_on_the_job_still_does_not_gate() {
        for key in ["\"if\": false", "'continue-on-error': true"] {
            let yaml = GATING.replace("  test:\n", &format!("  test:\n    {key}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a job carrying `{key}` does not gate, quoted or not"
            );
        }
    }

    /// EVERY BLOCK-SCALAR SPELLING IS A BLOCK. `|` is not the only
    /// one: YAML's chomping and indentation indicators all open a
    /// block, and treating one as the command itself meant the block's
    /// contents were never read -- so changing `|` to `|-`, which
    /// preserves behaviour, made the guard report that nothing gated.
    #[test]
    fn a_run_block_is_read_whole_in_every_block_scalar_spelling() {
        for indicator in ["|", "|-", "|+", ">", ">-", ">+", "|2", ">2-"] {
            let yaml = format!(
                "on:\n  pull_request:\n    branches: [main]\njobs:\n  test:\n    steps:\n\
                 {}      - name: a block\n        run: {indicator}\n          set -euo pipefail\n\
                 {}          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                "", ""
            );
            assert_eq!(
                gating(&yaml).len(),
                1,
                "`run: {indicator}` opens a block, so the command inside it must be seen"
            );
        }
    }

    /// The flow-sequence and single-scalar spellings of `on:`, which
    /// put the triggers on the same line rather than in a block.
    #[test]
    fn the_inline_trigger_spellings_are_read_too() {
        let base = GATING.replace("on:\n  pull_request:\n    branches: [main]\n", "");
        for (spelling, gates) in [
            ("on: [push, pull_request]\n", true),
            ("on: [push]\n", false),
            ("on: pull_request\n", true),
            ("on: push\n", false),
            ("on:\n  - push\n  - pull_request\n", true),
            ("on:\n  - push\n", false),
        ] {
            let yaml = format!("{spelling}{base}");
            assert_eq!(
                !gating(&yaml).is_empty(),
                gates,
                "`{spelling:?}` should {} gate",
                if gates { "" } else { "not" }
            );
        }
    }

    /// A `run: |` block is read whole, so a command inside a shell
    /// loop is visible rather than seen as fragments.
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
            gating(yaml).len(),
            1,
            "a command inside a `run: |` block must be seen; several steps in this \
             repository's ci.yml are blocks like this one"
        );
    }

    /// The real `ci.yml` gates, read through the same function the
    /// guard uses. Distinct from the guard's own assertion: this one
    /// proves the PARSER copes with the real file's shape -- matrix
    /// strategies, `uses:`/`with:` mappings, comments between steps --
    /// rather than only with the fixtures above.
    #[test]
    fn the_real_ci_yml_still_parses_into_a_gating_step() {
        let workflow = super::read_or_panic(
            &super::manifest_dir()
                .join(".github")
                .join("workflows")
                .join("ci.yml"),
        );
        let chores = super::read_or_panic(&super::manifest_dir().join("chores.yml"));
        assert!(
            !super::gating_checking_debug_runs_via_chore(&workflow, &chores).is_empty(),
            "the real ci.yml must parse into at least one gating step, or the guard is \
             passing on a fixture and failing on the file it exists to read"
        );
    }
}

/// Following `chore <task>` into chores.yml: what resolves, and what a
/// task or a command item carries that stops it gating. Every test is
/// [`WORKFLOW`] and [`CHORES`] with one thing changed.
mod chore {
    use super::gating_checking_debug_runs_via_chore as gating;

    const WORKFLOW: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  unit:
    steps:
      - name: chore test:unit
        run: chore test:unit
";

    const CHORES: &str = "\
version: '3'
tasks:
  test:unit:
    cmds:
      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'
  test:
    cmds:
      - task: test:unit
      - cargo test --locked --release
";

    const UNIT_CMDS: &str =
        "    cmds:\n      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'\n";

    #[test]
    fn a_chore_step_resolving_to_a_checking_debug_run_gates() {
        assert_eq!(
            gating(WORKFLOW, CHORES),
            vec!["chore test:unit: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
            "the control must be counted, or every test below passes for the wrong reason"
        );
    }

    #[test]
    fn a_chore_step_reaching_the_run_through_a_nested_task_gates() {
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore test");
        assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
        assert_eq!(
            gating(&workflow, CHORES),
            vec![
                "chore test -> test:unit: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib"
                    .to_string()
            ],
            "`task: test:unit` runs the task, so its run is the step's run"
        );
    }

    #[test]
    fn a_chore_step_with_arguments_after_the_task_still_resolves() {
        for line in [
            "chore test:unit --force",
            "chore test:unit -- --nocapture",
            "CARGO_TERM_COLOR=always chore test:unit",
            "set -eu; chore test:unit --force",
            "~/.local/bin/chore test:unit  # the unit tier",
        ] {
            let workflow = WORKFLOW.replace("run: chore test:unit", &format!("run: {line}"));
            assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
            assert_eq!(
                gating(&workflow, CHORES).len(),
                1,
                "`{line}` runs test:unit"
            );
        }
    }

    /// chore may SKIP a task that carries any of these as up to date, and
    /// an up-to-date skip is a green step. Presence, not value.
    #[test]
    fn a_task_chore_may_skip_as_up_to_date_does_not_gate() {
        for key in [
            "sources: [src/lib.rs]",
            "generates: [target/done]",
            "status: ['true']",
            "sources: []",
        ] {
            let chores = CHORES.replace(UNIT_CMDS, &format!("    {key}\n{UNIT_CMDS}"));
            assert_ne!(chores, CHORES, "the mutation must actually apply");
            assert!(
                gating(WORKFLOW, &chores).is_empty(),
                "a task carrying `{key}` may be skipped as up to date, so its run cannot be \
                 what makes the gate see an overflow"
            );
        }
    }

    /// The same, on a task ON THE PATH rather than the one holding the run.
    #[test]
    fn a_skippable_task_on_the_path_does_not_gate() {
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore test");
        let chores = CHORES.replace(
            "  test:\n    cmds:\n",
            "  test:\n    sources: [Cargo.toml]\n    cmds:\n",
        );
        assert_ne!(chores, CHORES, "the mutation must actually apply");
        assert_eq!(gating(&workflow, CHORES).len(), 1, "control");
        assert!(
            gating(&workflow, &chores).is_empty(),
            "if `test` is skipped, the `test:unit` it would have run is skipped too"
        );
    }

    #[test]
    fn a_cmd_carrying_ignore_error_does_not_gate() {
        let chores = CHORES.replace(
            "      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'\n",
            "      - cmd: 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib'\n        ignore_error: true\n",
        );
        assert_ne!(chores, CHORES, "the mutation must actually apply");
        assert!(
            gating(WORKFLOW, &chores).is_empty(),
            "a command whose failure chore ignores is a run whose result nothing reads"
        );

        // The control: the same mapping spelling, without the key, gates.
        let control = chores.replace("        ignore_error: true\n", "");
        assert_eq!(
            gating(WORKFLOW, &control).len(),
            1,
            "a `cmd:` mapping is a command"
        );

        // And on the `task:` item that reaches it.
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore test");
        let chores = CHORES.replace(
            "      - task: test:unit\n",
            "      - task: test:unit\n        ignore_error: false\n",
        );
        assert_ne!(chores, CHORES, "the mutation must actually apply");
        assert!(
            gating(&workflow, &chores).is_empty(),
            "`ignore_error: false` too: the key's presence, not its value"
        );
    }

    #[test]
    fn a_chore_step_carrying_if_still_does_not_gate() {
        let workflow = WORKFLOW.replace(
            "        run: chore test:unit\n",
            "        run: chore test:unit\n        if: false\n",
        );
        assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
        assert!(
            gating(&workflow, CHORES).is_empty(),
            "resolving the task happens only for a step that gates"
        );
    }

    #[test]
    fn a_chore_line_that_is_a_comment_or_inverted_does_not_resolve() {
        for run in [
            "run: |\n          # chore test:unit\n          echo nothing",
            "run: '! chore test:unit'",
            "run: echo chore test:unit",
        ] {
            let workflow = WORKFLOW.replace("run: chore test:unit", run);
            assert_ne!(workflow, WORKFLOW, "the mutation must actually apply");
            assert!(
                gating(&workflow, CHORES).is_empty(),
                "`{run}` does not run test:unit and require it to pass"
            );
        }
    }

    #[test]
    #[should_panic(
        expected = "`chore test:unti` (from a workflow step) names no task in chores.yml"
    )]
    fn a_chore_step_naming_a_task_that_does_not_exist_panics() {
        gating(
            &WORKFLOW.replace("chore test:unit", "chore test:unti"),
            CHORES,
        );
    }

    #[test]
    #[should_panic(expected = "`chore test:unti` (from test) names no task in chores.yml")]
    fn a_task_item_naming_a_task_that_does_not_exist_panics() {
        gating(
            &WORKFLOW.replace("run: chore test:unit", "run: chore test"),
            &CHORES.replace("- task: test:unit", "- task: test:unti"),
        );
    }

    /// A cycle is chore's error to report; the guard must just not hang.
    #[test]
    fn a_task_cycle_terminates() {
        let chores = format!("{CHORES}  a:\n    cmds:\n      - task: b\n      - task: test:unit\n  b:\n    cmds:\n      - task: a\n");
        let workflow = WORKFLOW.replace("run: chore test:unit", "run: chore a");
        assert_eq!(gating(&workflow, &chores).len(), 1);
    }

    /// `scripts/test.sh` is read as `cargo test`, which is only true while
    /// it ends by handing cargo every argument it was given. It may run
    /// cargo inside the harness's `vm.sh session`, which runs the command
    /// it is given as it is, arguments and status included; that prefix,
    /// spelled exactly so, is the only thing allowed in front of cargo.
    #[test]
    fn scripts_test_sh_is_still_cargo_test_with_its_arguments() {
        const SESSION: &str = "${session[@]+\"${session[@]}\"} ";
        let script = super::read_or_panic(&super::manifest_dir().join(super::TEST_WRAPPER));
        let last = script
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.strip_prefix(SESSION).unwrap_or(line));
        assert_eq!(
            last,
            Some("cargo test --features cli \"$@\""),
            "{} no longer ends in `cargo test --features cli \"$@\"`, so \
             checking_debug_runs must stop reading it as `cargo test`",
            super::TEST_WRAPPER
        );
        let chores = CHORES.replace(
            "cargo test --locked --lib",
            "scripts/test.sh --locked --lib",
        );
        assert_eq!(
            gating(WORKFLOW, &chores).len(),
            1,
            "the wrapper in debug gates"
        );
        for release in ["--release --lib", "-r --lib"] {
            let chores = CHORES.replace(
                "cargo test --locked --lib",
                &format!("scripts/test.sh --locked {release}"),
            );
            assert!(
                gating(WORKFLOW, &chores).is_empty(),
                "the wrapper with `{release}` is release"
            );
        }
    }

    fn real(path: &[&str]) -> String {
        let mut full = super::manifest_dir();
        full.extend(path);
        super::read_or_panic(&full)
    }

    /// THE GUARD BITES ON THE REAL FILES, mutated in memory.
    ///
    /// Note what it takes: dropping the `unit` job's `chore test:unit` is
    /// NOT enough on its own, because the `test` job's `chore test` runs
    /// `task: test:unit` as well -- in debug, with the handshake -- so the
    /// PR gate still sees an overflow, on both architectures. Only both
    /// gone, or the handshake gone from the task, or the task made
    /// skippable, leaves nothing.
    #[test]
    fn the_real_files_stop_gating_when_the_run_is_removed() {
        let workflow = real(&[".github", "workflows", "ci.yml"]);
        let chores = real(&["chores.yml"]);
        assert!(!gating(&workflow, &chores).is_empty(), "control");

        let no_unit_step = workflow.replace("run: chore test:unit\n", "run: 'true'\n");
        assert_ne!(no_unit_step, workflow, "the mutation must actually apply");
        assert!(
            gating(&no_unit_step, &chores)
                .iter()
                .all(|run| run.starts_with("chore test -> test:unit: ")),
            "without the unit job's step, only `chore test` reaches the run"
        );
        let neither = no_unit_step.replace("run: chore test\n", "run: 'true'\n");
        assert_ne!(neither, no_unit_step, "the mutation must actually apply");
        assert!(
            gating(&neither, &chores).is_empty(),
            "no step reaches test:unit"
        );

        let no_handshake = chores.replace("EXPECT_OVERFLOW_CHECKS=1 ", "");
        assert_ne!(no_handshake, chores, "the mutation must actually apply");
        assert!(
            gating(&workflow, &no_handshake).is_empty(),
            "test:unit without EXPECT_OVERFLOW_CHECKS=1 asks the build nothing"
        );

        let skippable = chores.replace(
            "  test:unit:\n",
            "  test:unit:\n    sources: ['src/**/*.rs']\n",
        );
        assert_ne!(skippable, chores, "the mutation must actually apply");
        assert!(
            gating(&workflow, &skippable).is_empty(),
            "a test:unit chore may skip as up to date gates nothing"
        );
    }
}

/// The parser is the part of this that can rot, checked against each
/// shape it has to tell apart.
mod parser {
    use super::checking_debug_runs;

    /// The trap this repository's own `ci.yml` contains: the debug
    /// command quoted verbatim in the comment explaining the step.
    /// `-r` IS `--release`, IN EVERY SPELLING CLAP ACCEPTS (#158). Each of
    /// these compiles with overflow checks off and counted as the debug
    /// run; the controls beside them are not release and still count.
    #[test]
    fn the_short_release_flag_does_not_count_in_any_spelling() {
        for line in [
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -r --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -qr --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -rq --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -j4 -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -j 4 -r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features x -r",
            "EXPECT_OVERFLOW_CHECKS=1 /usr/bin/cargo test --locked -r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo +1.95.0 test --locked -r --lib",
            "true&&EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib 2>&1 -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features 'a;b' -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features \"a&&b\" -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features a\\;b -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked '-r'",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked \"-qr\"",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked \\-r",
        ] {
            assert_eq!(
                checking_debug_runs(line),
                Vec::<String>::new(),
                "{line} builds the release profile"
            );
        }
        for line in [
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets -- -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --features r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -F r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -pr --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -j r --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib && rm -rf build",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib; echo -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib&&rm -rf build",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib;echo -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib|tee -r",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -- '-r'",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib && echo 'cargo test -r'",
        ] {
            assert_eq!(
                checking_debug_runs(line).len(),
                1,
                "{line}: the r is a value or the harness's, and the run is debug"
            );
        }
    }

    #[test]
    fn a_debug_run_quoted_in_a_comment_does_not_count() {
        let yaml = "\
jobs:
  test:
    steps:
      # Measured on this branch:
      #     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib   ->  EXIT=101
      - run: cargo test --locked --release
";
        assert_eq!(
            checking_debug_runs(yaml),
            Vec::<String>::new(),
            "a debug command quoted inside a comment is documentation, not a run"
        );
    }

    #[test]
    fn a_real_checking_debug_run_counts() {
        let yaml = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --release
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            checking_debug_runs(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A debug step present but never asked to check anything buys
    /// nothing over deleting it -- the whole point of the handshake.
    #[test]
    fn a_debug_run_without_the_handshake_does_not_count() {
        let yaml = "      - run: cargo test --locked --lib\n";
        assert_eq!(
            checking_debug_runs(yaml),
            Vec::<String>::new(),
            "the step runs but nothing checks the build it produced"
        );
    }

    /// A handshake on a release run proves nothing: the checks are
    /// legitimately off there.
    #[test]
    fn the_handshake_on_a_release_run_does_not_count() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --release\n";
        assert_eq!(checking_debug_runs(yaml), Vec::<String>::new());
    }

    /// An inline trailing comment naming `--release` must not disqualify
    /// a genuine debug run.
    #[test]
    fn a_trailing_comment_naming_release_does_not_disqualify_a_debug_run() {
        let yaml =
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib  # deliberately not --release\n";
        assert_eq!(
            checking_debug_runs(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A STEP THAT RUNS NO COMMAND MAY BE CONDITIONAL (#264).
    ///
    /// `actions/upload-artifact` has no `run:`. It cannot pass, cannot
    /// mask a failure and cannot make a red job green — and without
    /// `if: always()` GitHub skips it whenever an earlier step failed,
    /// which is the only time keeping the log is worth anything. The
    /// blanket ban made the quiet suite's logs unkeepable on exactly
    /// the runs that need them.
    fn check_job(steps: &str) {
        let text = format!(
            "on:\n  pull_request:\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n{steps}"
        );
        let document = super::load_document(&text, std::path::Path::new("synthetic.yml"));
        let job = super::job(&document, "test", std::path::Path::new("synthetic.yml"));
        super::assert_unconditional(job, "test", "ci.yml");
    }

    #[test]
    fn a_step_that_runs_nothing_may_carry_a_condition() {
        check_job(
            "      - uses: actions/upload-artifact@v4\n        if: always()\n        with:\n          name: logs\n",
        );
    }

    #[test]
    #[should_panic(expected = "must not be conditional")]
    fn a_step_that_runs_a_command_may_not() {
        check_job("      - run: cargo test --locked --release\n        if: always()\n");
    }

    /// And `continue-on-error:` stays refused whatever the step is: on
    /// an upload it says a failed upload does not matter, which is a
    /// different claim from "run this even after a failure".
    #[test]
    #[should_panic(expected = "continue-on-error")]
    fn a_step_that_runs_nothing_may_still_not_be_allowed_to_fail() {
        check_job("      - uses: actions/upload-artifact@v4\n        continue-on-error: true\n");
    }

    /// A profile named another way still disqualifies the run.
    #[test]
    fn a_profile_flag_disqualifies_a_run_even_with_the_handshake() {
        let yaml =
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --profile release-with-debug --lib\n";
        assert_eq!(checking_debug_runs(yaml), Vec::<String>::new());
    }
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

/// Which `rust-fs-core` this crate is built against, as stated in one file.
///
/// Three spellings, because the pin is a dependency version in a
/// manifest, an environment variable in a workflow, and once an
/// argument to `git clone`. All three are returned normalised without
/// the leading `v`, paired with the spelling that produced them so a
/// failure names what to edit.
///
/// A comment is not a declaration. `chores.yml` explains its pin in
/// prose that names three versions, and a scan that read those would
/// report a disagreement nobody can fix.
///
/// A value that is not a literal version is not a declaration either: a
/// clone of `--branch "$FS_CORE_REF"` names a variable declared
/// elsewhere, not a second, independent pin.
fn am_fs_core_versions_declared(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.contains("rust-fs-core") {
            if let Some(v) = quoted_value_after(line, "version") {
                found.push(("the rust-fs-core dependency's `version`".to_string(), v));
            }
        }
        if let Some(rest) = line.split_once("FS_CORE_REF:") {
            if let Some(v) = as_version(rest.1) {
                found.push(("`FS_CORE_REF`".to_string(), v));
            }
        }
        if line.contains("rust-fs-core") {
            if let Some(rest) = line.split_once("--branch ") {
                if let Some(v) = as_version(rest.1.split_whitespace().next().unwrap_or("")) {
                    found.push(("the `git clone --branch` ref".to_string(), v));
                }
            }
        }
    }
    found
}

/// The value of `key = "..."` on a line, if it is there.
fn quoted_value_after(line: &str, key: &str) -> Option<String> {
    let after = line.split_once(key)?.1;
    let after = after.trim_start().strip_prefix('=')?.trim_start();
    let inner = after.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// `v0.2.13`, `"0.2.13"` and `0.2.13` are the same pin; `"$FS_CORE_REF"`
/// and `main` are not pins at all.
fn as_version(raw: &str) -> Option<String> {
    let t = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    let t = t.strip_prefix('v').unwrap_or(t);
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        Some(t.to_string())
    } else {
        None
    }
}

/// Every file that declares one.
///
/// A fixed list rather than a walk: the point is that each of these is
/// known to carry a pin, so one that stops carrying it is a failure
/// rather than a file the walk no longer visits. `release.yml` is not
/// here because it declares nothing: it runs `chore siblings`, which
/// reads `FS_CORE_REF` from `chores.yml`.
const FILES_THAT_PIN_AM_FS_CORE: &[&str] = &[
    "Cargo.toml",
    "fuzz/Cargo.toml",
    "chores.yml",
    ".github/workflows/fuzz.yml",
];

/// Every file that names the `rust-fs-core` this crate is built against
/// names the same one (#144).
///
/// Four files carry that version and nothing compared them.
/// `fuzz/Cargo.toml` was left at `0.2.10` when the crate moved to
/// `0.2.13`, and nothing failed, because `version = "0.2.10"` is
/// `^0.2.10` and 0.2.13 satisfies it -- it would have stayed wrong until
/// core reached 0.3 and broken the fuzz build then, detached from the
/// change that caused it. The nightly fuzz run went red on the 22nd to
/// the 26th of September on `caching_device.rs:167`, `attempt to add
/// with overflow`, a bug core had already fixed and shipped, because
/// the workflow's clone lagged the same way.
///
/// The count is asserted as well as the agreement. A scan that stopped
/// matching would otherwise report perfect agreement among nothing,
/// which is the failure this whole file is written against.
#[test]
fn every_declaration_of_the_am_fs_core_pin_names_the_same_version() {
    let root = manifest_dir();
    let mut declared: Vec<(&str, String, String)> = Vec::new();
    let mut silent = Vec::new();

    for rel in FILES_THAT_PIN_AM_FS_CORE {
        let found = am_fs_core_versions_declared(&read_or_panic(&root.join(rel)));
        if found.is_empty() {
            silent.push(*rel);
        }
        for (spelling, version) in found {
            declared.push((rel, spelling, version));
        }
    }

    assert!(
        silent.is_empty(),
        "these files are listed as declaring the rust-fs-core pin and no longer do: \
         {silent:?}. Either the pin moved somewhere this guard does not read -- in \
         which case add it to FILES_THAT_PIN_AM_FS_CORE -- or the file stopped \
         pinning the sibling and now builds against whatever is checked out."
    );
    assert!(
        declared.len() >= FILES_THAT_PIN_AM_FS_CORE.len(),
        "found {} declarations across {} files; every listed file must carry one",
        declared.len(),
        FILES_THAT_PIN_AM_FS_CORE.len()
    );

    let agreed = &declared[0].2;
    let disagreeing: Vec<String> = declared
        .iter()
        .filter(|(_, _, v)| v != agreed)
        .map(|(file, spelling, v)| format!("{file}: {spelling} says {v}"))
        .collect();
    assert!(
        disagreeing.is_empty(),
        "the rust-fs-core pin disagrees across the files that declare it. \
         {}: {} says {agreed}, but {disagreeing:?}. A bump has to move every \
         one of them: one that does not builds part of this repository -- the \
         fuzz crate, last time -- against a release the rest of it has left \
         behind, and nothing fails until that release stops being compatible.",
        declared[0].0,
        declared[0].1,
    );
}

mod core_pin_parser {
    use super::{am_fs_core_versions_declared, as_version};

    /// The real dependency line, in both manifests' shape.
    #[test]
    fn a_path_dependency_declares_its_version() {
        let toml = "rust-fs-core = { path = \"../../rust-fs-core\", version = \"0.2.10\" }\n";
        assert_eq!(
            am_fs_core_versions_declared(toml),
            vec![(
                "the rust-fs-core dependency's `version`".to_string(),
                "0.2.10".to_string()
            )],
        );
    }

    /// The chores.yml spelling, with and without the `v`.
    #[test]
    fn the_ref_variable_declares_its_version() {
        assert_eq!(
            am_fs_core_versions_declared("  FS_CORE_REF: v0.2.13\n"),
            vec![("`FS_CORE_REF`".to_string(), "0.2.13".to_string())],
        );
    }

    /// The clone spelling, which is how fuzz.yml carries its pin.
    #[test]
    fn a_clone_branch_declares_its_version() {
        let line = "run: git clone --depth 1 --branch v0.2.10 \
                    https://github.com/antimatter-studios/rust-fs-core.git ../rust-fs-core\n";
        assert_eq!(
            am_fs_core_versions_declared(line),
            vec![(
                "the `git clone --branch` ref".to_string(),
                "0.2.10".to_string()
            )],
        );
    }

    /// `chores.yml` explains its pin in prose naming three versions.
    /// Reading those would report a disagreement with nothing to edit.
    #[test]
    fn a_comment_naming_a_version_is_not_a_declaration() {
        let yaml = "  # v0.2.13: the release whose wrapper fails quietly.\n  \
                    # v0.2.11 and v0.2.12 satisfy the dependency but not this.\n";
        assert!(am_fs_core_versions_declared(yaml).is_empty());
    }

    /// A clone of `--branch "$FS_CORE_REF"` names the variable, not a
    /// second pin, and counting it would make the guard unsatisfiable.
    #[test]
    fn a_branch_that_is_a_variable_is_not_a_declaration() {
        let line = "run: git clone --depth 1 --branch \"$FS_CORE_REF\" \
                    https://github.com/antimatter-studios/rust-fs-core.git ../rust-fs-core\n";
        assert!(am_fs_core_versions_declared(line).is_empty());
    }

    /// A branch name is not a version. A pin moved to `main` is a pin
    /// abandoned, and the empty result makes its file silent, which
    /// the guard fails on separately.
    #[test]
    fn a_branch_name_is_not_a_version() {
        assert_eq!(as_version("main"), None);
        assert_eq!(as_version("v0.2"), None);
        assert_eq!(as_version("v0.2.13"), Some("0.2.13".to_string()));
    }
}

/// Every commit on `main` gets a whole CI run. With `cancel-in-progress: true`
/// for every event, a merge cancelled the run of the merge before it, and
/// `ci-ok` failed on jobs that were cancelled rather than broken: a red mark on
/// a commit nobody had finished testing. And GitHub keeps one pending run per
/// concurrency group, so a third merge cancels a second one still queued even
/// with cancelling switched off. So a push is a group of its own, keyed by its
/// commit, and only a pull request's superseded run is cancelled.
#[test]
fn ci_cancels_only_a_pull_requests_superseded_run() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/ci.yml");
    let text = std::fs::read_to_string(&path).expect("read ci.yml");
    let (mut group, mut cancel) = (None, None);
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("concurrency:") {
            inside = true;
            continue;
        }
        if inside {
            if !line.starts_with(' ') && !line.trim().is_empty() {
                break;
            }
            if let Some(v) = line.trim().strip_prefix("group:") {
                group = Some(v.trim().to_string());
            }
            if let Some(v) = line.trim().strip_prefix("cancel-in-progress:") {
                cancel = Some(v.trim().to_string());
            }
        }
    }
    assert_eq!(
        cancel.as_deref(),
        Some("${{ github.event_name == 'pull_request' }}"),
        "ci.yml cancels a run that is not a pull request's"
    );
    let group = group.expect("ci.yml declares a concurrency group");
    assert!(
        group.contains("github.event.pull_request.number || github.sha"),
        "ci.yml's concurrency group is shared between pushes, so a queued push run is \
         cancelled by the next: {group}"
    );
}
