//! One check gates a pull request, and it stands for every job (#123).
//!
//! Branch protection names checks, and a check is a job name. Naming each
//! job means the list has to be edited whenever a job is renamed, split
//! into a matrix leg, or added — and until someone does, the new work is
//! required by nobody. The opposite spelling is worse: a required check no
//! job produces reads as permanently pending, and with `enforce_admins` on
//! nothing merges and there is no failure to point at.
//!
//! So protection names one job, `ci-ok`, which `needs:` every other job and
//! fails unless each concluded success. This is what keeps that true: every
//! job in `ci.yml` has to be in its `needs`, and `.github-guard` has to
//! require it and nothing else.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// The workflow's job ids: the keys one indent inside `jobs:`.
fn jobs(workflow: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_jobs = false;
    for line in workflow.lines() {
        if line.starts_with("jobs:") {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        // A top-level key ends the jobs block.
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.trim_start().starts_with('#')
        {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && line.trim().ends_with(':') && !line.trim_start().starts_with('#') {
            out.push(line.trim().trim_end_matches(':').to_string());
        }
    }
    out
}

/// What `ci-ok` says it needs, from `needs: [a, b]` or a block list.
fn needs_of(workflow: &str, job: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut in_block_list = false;
    for line in workflow.lines() {
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && line.trim() == format!("{job}:") {
            inside = true;
            continue;
        }
        if inside && indent == 2 && line.trim().ends_with(':') {
            break;
        }
        if !inside {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("needs:") {
            let rest = rest.trim();
            if rest.starts_with('[') {
                out.extend(
                    rest.trim_matches(['[', ']'].as_slice())
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            } else if rest.is_empty() {
                in_block_list = true;
            } else {
                out.push(rest.to_string());
            }
            continue;
        }
        if in_block_list {
            match trimmed.strip_prefix("- ") {
                Some(one) => out.push(one.trim().to_string()),
                None => in_block_list = false,
            }
        }
    }
    out
}

/// The checks `.github-guard` declares as required.
fn required_checks() -> Vec<String> {
    std::fs::read_to_string(repo().join(".github-guard"))
        .expect(".github-guard")
        .lines()
        .filter_map(|l| l.split_once("required ="))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .collect()
}

#[test]
fn the_aggregate_job_needs_every_other_job() {
    let workflow =
        std::fs::read_to_string(repo().join(".github/workflows/ci.yml")).expect("ci.yml");
    let jobs = jobs(&workflow);
    assert!(
        jobs.contains(&"ci-ok".to_string()),
        "ci.yml has no ci-ok job, so protection has to name every job by hand: {jobs:?}"
    );
    let needs = needs_of(&workflow, "ci-ok");
    for job in jobs.iter().filter(|j| *j != "ci-ok") {
        assert!(
            needs.contains(job),
            "ci-ok does not need {job}, so that job gates nothing: needs {needs:?}"
        );
    }
    assert!(
        !needs.is_empty(),
        "ci-ok needs nothing, so it says every job succeeded while asking none of them"
    );
}

#[test]
fn protection_requires_the_aggregate_and_nothing_else() {
    let required = required_checks();
    assert_eq!(
        required,
        vec!["ci-ok".to_string()],
        "protection should name the aggregate alone: a job named here as well drifts when \
         it is renamed, and one named here but not in ci.yml is required by a job that \
         never reports"
    );
}

/// The aggregate has to run even when what it watches did not: a job that
/// was skipped or cancelled is not a job that passed, and an aggregate that
/// only runs on success cannot say so.
#[test]
fn the_aggregate_runs_whatever_happened() {
    let workflow =
        std::fs::read_to_string(repo().join(".github/workflows/ci.yml")).expect("ci.yml");
    let mut inside = false;
    let mut always = false;
    for line in workflow.lines() {
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && line.trim() == "ci-ok:" {
            inside = true;
            continue;
        }
        if inside && indent == 2 && line.trim().ends_with(':') {
            break;
        }
        if inside && line.trim() == "if: always()" {
            always = true;
        }
    }
    assert!(
        always,
        "ci-ok does not carry `if: always()`, so a cancelled or skipped job leaves it \
         skipped too — and a skipped required check never reports"
    );
}
