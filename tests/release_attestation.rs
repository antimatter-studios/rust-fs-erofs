//! The release workflow attests the crate it publishes.
//!
//! A version on crates.io says nothing about where it was built: anyone
//! holding a publish token could have uploaded it from their own machine.
//! `release.yml` therefore packages the crate, publishes it, checks that
//! the file it packaged is byte-for-byte the one crates.io serves, and
//! signs a build-provenance attestation over that file with the
//! workflow's own identity. The same `.crate` is attached to the GitHub
//! release for the tag, so anyone can check a download with
//!
//! ```text
//! gh attestation verify <crate> --repo <owner>/<repo> \
//!     --signer-workflow <owner>/<repo>/.github/workflows/release.yml
//! ```
//!
//! Nothing else notices if that step goes. The workflow runs only on a
//! version tag, and a release without an attestation publishes exactly
//! as green as one with it; the gap would surface the first time someone
//! tried to verify a download, long after the version was taken. This
//! file makes the loss loud on the pull request that causes it.
//!
//! It also keeps the privileges where they are needed. The attesting job
//! must be able to mint an OIDC token, write an attestation and attach a
//! release asset; no other job, and not the workflow as a whole, may
//! hold any of those grants.
//!
//! The workflow is PARSED rather than scanned, so a step name, a comment
//! or a quoted string cannot satisfy a check meant for a real step.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::Path;

const WORKFLOW: &str = ".github/workflows/release.yml";

/// The action that signs the attestation, up to its `@`.
const ATTEST: &str = "actions/attest-build-provenance@";

/// The grants the attesting job needs, each at `write`: an OIDC token
/// to sign with, the attestation store, and the release to attach to.
const GRANTS: &[&str] = &["id-token", "attestations", "contents"];

fn load(yaml: &str) -> Yaml<'static> {
    let mut docs = Yaml::load_from_str(yaml).expect("the workflow parses as YAML");
    assert_eq!(docs.len(), 1, "one YAML document");
    docs.remove(0)
}

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(WORKFLOW);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {WORKFLOW}: {e}"))
}

/// The lines of a `run:` script that are commands, not comments.
fn commands(step: &Yaml) -> Vec<String> {
    let Some(run) = step.as_mapping_get("run").and_then(Yaml::as_str) else {
        return Vec::new();
    };
    run.replace("\\\n", " ")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The commands in a step that start with `program`, so an `echo` or
/// a string mentioning it does not count.
fn invocations(step: &Yaml, program: &str) -> Vec<String> {
    commands(step)
        .into_iter()
        .filter(|c| c.starts_with(program))
        .collect()
}

fn runs(step: &Yaml, program: &str) -> bool {
    !invocations(step, program).is_empty()
}

/// Every grant in `permissions` that is `write`, by name. `write-all`
/// grants every one.
fn write_grants(permissions: Option<&Yaml>) -> Vec<String> {
    let Some(permissions) = permissions else {
        return Vec::new();
    };
    if permissions.as_str() == Some("write-all") {
        return GRANTS.iter().map(|g| (*g).to_owned()).collect();
    }
    let Some(map) = permissions.as_mapping() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, v)| v.as_str() == Some("write"))
        .filter_map(|(k, _)| k.as_str().map(str::to_owned))
        .filter(|k| GRANTS.contains(&k.as_str()))
        .collect()
}

fn is_full_sha(pin: &str) -> bool {
    pin.len() == 40
        && pin
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Everything wrong with how `yaml` attests what it publishes; empty
/// when nothing is.
fn attestation_gaps(yaml: &str) -> Vec<String> {
    let doc = load(yaml);
    let mut gaps = Vec::new();
    for grant in write_grants(doc.as_mapping_get("permissions")) {
        gaps.push(format!(
            "the workflow-level permissions grant {grant}: write to every job"
        ));
    }
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let mut attesting = 0;
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or("?");
        let steps: Vec<&Yaml> = job
            .as_mapping_get("steps")
            .and_then(Yaml::as_sequence)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        let granted = write_grants(job.as_mapping_get("permissions"));
        let attest_at = steps.iter().position(|s| {
            s.as_mapping_get("uses")
                .and_then(Yaml::as_str)
                .is_some_and(|u| u.starts_with(ATTEST))
        });
        let Some(at) = attest_at else {
            // Core's release-cli workflow attests the tarballs inside the
            // job it runs, so the calling job holds the grants it passes
            // on; release_cli_gaps holds that call to its own rules.
            if job
                .as_mapping_get("uses")
                .and_then(Yaml::as_str)
                .is_some_and(|u| u.starts_with(CORE_RELEASE_CLI))
            {
                continue;
            }
            for grant in granted {
                gaps.push(format!(
                    "job {name} attests nothing but holds {grant}: write"
                ));
            }
            continue;
        };
        attesting += 1;
        let step = steps[at];
        let uses = step
            .as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .unwrap_or("");
        let pin = &uses[ATTEST.len()..];
        if !is_full_sha(pin) {
            gaps.push(format!(
                "job {name} uses {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        let subject = step
            .as_mapping_get("with")
            .and_then(|w| w.as_mapping_get("subject-path"))
            .and_then(Yaml::as_str)
            .unwrap_or("");
        if !subject.contains(".crate") {
            gaps.push(format!(
                "job {name} attests {subject:?}, not the packaged .crate"
            ));
        }
        if !steps[..at].iter().any(|s| runs(s, "cargo package")) {
            gaps.push(format!("job {name} attests before any `cargo package`"));
        }
        if !steps[..at].iter().any(|s| runs(s, "cargo publish")) {
            gaps.push(format!(
                "job {name} attests before `cargo publish`, so what it signs is not \
                 known to be what was published"
            ));
        }
        if !steps[at + 1..].iter().any(|s| {
            invocations(s, "gh release upload")
                .iter()
                .any(|c| c.contains(".crate"))
        }) {
            gaps.push(format!(
                "job {name} does not attach the attested .crate to the GitHub release"
            ));
        }
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!("job {name} attests without {grant}: write"));
            }
        }
    }
    if attesting == 0 {
        gaps.push(format!("no job in the workflow uses {ATTEST}<sha>"));
    }
    gaps
}

#[test]
fn the_release_workflow_attests_the_crate_it_publishes() {
    let gaps = attestation_gaps(&workflow());
    assert!(
        gaps.is_empty(),
        "{WORKFLOW} must attest the .crate it publishes, from the one job that \
         publishes it, with only that job privileged: {gaps:#?}"
    );
}

/// The reader answers for the inputs it is meant to catch, and not for
/// the ones it is not.
#[test]
fn the_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         jobs:\n  test:\n    steps:\n      - run: cargo test\n\
         \x20 publish:\n    permissions:\n      id-token: write\n      attestations: write\n      contents: write\n\
         \x20   steps:\n      - run: cargo package --no-verify\n      - run: cargo publish\n\
         \x20     - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: target/package/*.crate\n\
         \x20     - run: gh release upload \"$GITHUB_REF_NAME\" target/package/*.crate --clobber\n"
    );
    assert_eq!(attestation_gaps(&good), Vec::<String>::new(), "{good}");

    let expect = |yaml: String, want: &str| {
        let gaps = attestation_gaps(&yaml);
        assert!(
            gaps.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {gaps:#?} for\n{yaml}"
        );
    };
    // The step gone entirely, or only named in a comment.
    let no_step = good.replace(&format!("      - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: target/package/*.crate\n"), "      # uses: actions/attest-build-provenance\n");
    expect(no_step, "no job in the workflow uses");
    // Pinned to a tag.
    expect(good.replace(sha, "v4.2.2"), "pin a full commit SHA");
    // Each grant dropped in turn.
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("attests without {grant}: write"),
        );
    }
    // A grant hoisted to the whole workflow.
    expect(
        good.replace(
            "permissions:\n  contents: read\n",
            "permissions:\n  id-token: write\n",
        ),
        "workflow-level permissions grant id-token",
    );
    expect(
        good.replace(
            "permissions:\n  contents: read\n",
            "permissions: write-all\n",
        ),
        "workflow-level permissions grant attestations",
    );
    // The call to core's release-cli holds the grants it passes on; any
    // other called workflow holding one is still refused.
    let cli = format!(
        "{good}\x20 cli:\n    needs: [test, publish]\n\
         \x20   permissions:\n      id-token: write\n\
         \x20   uses: {CORE_RELEASE_CLI}{sha}\n"
    );
    assert_eq!(attestation_gaps(&cli), Vec::<String>::new(), "{cli}");
    expect(
        cli.replace(CORE_RELEASE_CLI, "someone/else/.github/workflows/x.yml@"),
        "job cli attests nothing but holds id-token: write",
    );
    // A job that attests nothing, holding a grant.
    expect(
        good.replace(
            "  test:\n    steps:",
            "  test:\n    permissions:\n      id-token: write\n    steps:",
        ),
        "job test attests nothing but holds id-token: write",
    );
    // Signing before publishing, or something other than the crate.
    expect(
        good.replace("      - run: cargo publish\n", "")
            .replace("--clobber\n", "--clobber\n      - run: cargo publish\n"),
        "attests before `cargo publish`",
    );
    expect(
        good.replace(
            "subject-path: target/package/*.crate",
            "subject-path: Cargo.toml",
        ),
        "not the packaged .crate",
    );
    expect(
        good.replace(
            "      - run: cargo package --no-verify\n",
            "      - run: echo '# cargo package'\n",
        ),
        "attests before any `cargo package`",
    );
    // Not attached to the release.
    expect(
        good.replace("gh release upload", "echo gh-release-upload"),
        "does not attach the attested .crate",
    );
}

/// The reusable workflow that packages, attests and attaches the
/// command-line tarballs, up to its `@`. rust-fs-core holds the one copy
/// (#181); this repository only calls it.
const CORE_RELEASE_CLI: &str = "antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml@";

/// A file of this repository, read whole.
fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The value of `key:` in chores.yml's `vars:`, unquoted.
fn chores_var(key: &str) -> String {
    let chores = read("chores.yml");
    chores
        .lines()
        .find_map(|l| l.strip_prefix(&format!("  {key}:")))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_owned())
        .unwrap_or_else(|| panic!("chores.yml has no {key}"))
}

/// The toolchain rust-toolchain.toml pins.
fn pinned_toolchain() -> String {
    read("rust-toolchain.toml")
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel"))
        .map(|v| {
            v.trim_start_matches([' ', '='])
                .trim()
                .trim_matches('"')
                .to_owned()
        })
        .expect("rust-toolchain.toml pins a channel")
}

/// Everything wrong with how `yaml` hands the tarballs to core's
/// release-cli workflow; empty when nothing is.
fn release_cli_gaps(yaml: &str, core_ref: &str, toolchain: &str) -> Vec<String> {
    let doc = load(yaml);
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let mut gaps = Vec::new();
    let callers: Vec<(&str, &Yaml)> = jobs
        .iter()
        .filter(|(_, job)| {
            job.as_mapping_get("uses")
                .and_then(Yaml::as_str)
                .is_some_and(|u| u.starts_with(CORE_RELEASE_CLI))
        })
        .map(|(k, job)| (k.as_str().unwrap_or("?"), job))
        .collect();
    if callers.len() != 1 {
        gaps.push(format!(
            "{} jobs call {CORE_RELEASE_CLI}<sha>, not exactly one",
            callers.len()
        ));
    }
    for (name, job) in callers {
        let uses = job
            .as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .unwrap_or("");
        if !is_full_sha(&uses[CORE_RELEASE_CLI.len()..]) {
            gaps.push(format!(
                "job {name} uses {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        let needs: Vec<String> = match job.as_mapping_get("needs") {
            Some(n) if n.as_str().is_some() => vec![n.as_str().unwrap().to_owned()],
            Some(n) => n
                .as_sequence()
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        for need in ["test", "publish"] {
            if !needs.iter().any(|n| n == need) {
                gaps.push(format!("job {name} does not need job {need}"));
            }
        }
        let granted = write_grants(job.as_mapping_get("permissions"));
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!(
                    "job {name} calls release-cli without {grant}: write"
                ));
            }
        }
        let with = |key: &str| {
            job.as_mapping_get("with")
                .and_then(|w| w.as_mapping_get(key))
                .and_then(Yaml::as_str)
                .unwrap_or("")
                .to_owned()
        };
        if with("core-ref") != core_ref {
            gaps.push(format!(
                "job {name} passes core-ref {:?}, not chores.yml's FS_CORE_REF {core_ref:?}",
                with("core-ref")
            ));
        }
        if with("toolchain") != toolchain {
            gaps.push(format!(
                "job {name} passes toolchain {:?}, not rust-toolchain.toml's {toolchain:?}",
                with("toolchain")
            ));
        }
    }
    for (name, job) in jobs {
        let packages_locally = job
            .as_mapping_get("steps")
            .and_then(Yaml::as_sequence)
            .is_some_and(|st| {
                st.iter().any(|s| {
                    commands(s)
                        .iter()
                        .any(|c| c.contains("scripts/package-cli.sh"))
                        || s.as_mapping_get("with")
                            .and_then(|w| w.as_mapping_get("subject-path"))
                            .and_then(Yaml::as_str)
                            .is_some_and(|p| p.contains(".tar.gz"))
                })
            });
        if packages_locally {
            gaps.push(format!(
                "job {} packages or attests the tarballs itself, beside core's release-cli",
                name.as_str().unwrap_or("?")
            ));
        }
    }
    gaps
}

/// The tarballs are packaged, attested and attached by rust-fs-core's
/// release-cli workflow, pinned by commit SHA, at the core tag and
/// toolchain this repository pins, and no local copy of the packaging
/// remains (#181).
#[test]
fn the_tool_tarballs_are_released_by_core_release_cli() {
    let gaps = release_cli_gaps(&workflow(), &chores_var("FS_CORE_REF"), &pinned_toolchain());
    assert!(gaps.is_empty(), "{WORKFLOW}: {gaps:#?}");
    for copy in [
        "scripts/package-cli.sh",
        "tests/scripts/test-package-cli.sh",
    ] {
        assert!(
            !Path::new(env!("CARGO_MANIFEST_DIR")).join(copy).exists(),
            "{copy} is a local copy of rust-fs-core's packaging; run it as scripts/core.sh package-cli"
        );
    }
    let ci = read(".github/workflows/ci.yml");
    assert!(
        !ci.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .any(|l| l.contains("scripts/package-cli.sh")),
        "ci.yml runs a local scripts/package-cli.sh; run scripts/core.sh package-cli"
    );
}

#[test]
fn the_release_cli_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         jobs:\n  test:\n    steps:\n      - run: chore test\n\
         \x20 publish:\n    needs: test\n    steps:\n      - run: cargo publish\n\
         \x20 cli:\n    needs: [test, publish]\n\
         \x20   permissions:\n      contents: write\n      id-token: write\n      attestations: write\n\
         \x20   uses: {CORE_RELEASE_CLI}{sha} # v0.2.23\n\
         \x20   with:\n      core-ref: v0.2.23\n      toolchain: 1.95.0\n"
    );
    let gaps = |yaml: &str| release_cli_gaps(yaml, "v0.2.23", "1.95.0");
    assert_eq!(gaps(&good), Vec::<String>::new(), "{good}");
    let expect = |yaml: String, want: &str| {
        let got = gaps(&yaml);
        assert!(
            got.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {got:#?} for\n{yaml}"
        );
    };
    expect(good.replace(sha, "v0.2.23"), "pin a full commit SHA");
    expect(
        good.replace("needs: [test, publish]", "needs: [test]"),
        "does not need job publish",
    );
    expect(
        good.replace("needs: [test, publish]", "needs: publish"),
        "does not need job test",
    );
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("without {grant}: write"),
        );
    }
    expect(
        good.replace("core-ref: v0.2.23", "core-ref: v0.2.18"),
        "core-ref",
    );
    expect(
        good.replace("toolchain: 1.95.0", "toolchain: stable"),
        "toolchain",
    );
    expect(
        good.replace(
            CORE_RELEASE_CLI,
            "someone/else/.github/workflows/release-cli.yml@",
        ),
        "not exactly one",
    );
    expect(
        good.replace(
            "      - run: chore test\n",
            "      - run: scripts/package-cli.sh 1.0.0 x\n",
        ),
        "packages or attests the tarballs itself",
    );
}
