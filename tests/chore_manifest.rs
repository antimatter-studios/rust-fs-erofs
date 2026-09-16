//! `chores.yml`'s `staticlib` task fingerprints its own definition.
//!
//! `chore` decides whether `staticlib` is up to date from the content
//! checksums of the files in its `sources:` list. If `chores.yml` itself
//! is not in that list, an edit to the task -- a `cp` path, `vars.TRIPLE`,
//! a new step in `cmds:` -- does not change the fingerprint, so the next
//! `chore staticlib` prints `task: staticlib is up to date`, runs nothing,
//! and leaves the artefacts built by the previous definition in `dist/`.
//! Measured on #90 with chore 0.11.0: a changed triple kept shipping the
//! old architecture's archive, and a new first step never executed.
//!
//! Renaming `vars.LIBNAME` or `vars.OUT` is caught without the entry,
//! because `generates:` is rendered before the check; nothing else in
//! the task is.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::PathBuf;

fn chores_yml() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chores.yml");
    // Panic rather than skip: a guard that returns early on a missing
    // file asserts nothing and reports success.
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn field<'a, 'b>(node: &'a Yaml<'b>, name: &str) -> Option<&'a Yaml<'b>> {
    node.as_mapping()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// The `sources:` entries of one task, as strings.
fn task_sources(text: &str, task: &str) -> Vec<String> {
    let documents =
        Yaml::load_from_str(text).unwrap_or_else(|e| panic!("chores.yml does not parse: {e}"));
    let document = documents.first().expect("chores.yml holds no document");
    let task_node = field(document, "tasks")
        .and_then(|tasks| field(tasks, task))
        .unwrap_or_else(|| panic!("chores.yml has no `tasks.{task}`"));
    field(task_node, "sources")
        .and_then(Yaml::as_sequence)
        .unwrap_or_else(|| panic!("`tasks.{task}` has no `sources:` list"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("`tasks.{task}.sources` holds a non-string entry"))
                .to_string()
        })
        .collect()
}

#[test]
fn staticlib_sources_include_chores_yml() {
    let sources = task_sources(&chores_yml(), "staticlib");
    // Control: the list was actually read, so an empty or mis-parsed
    // list cannot pass by containing nothing.
    assert!(
        sources.iter().any(|s| s == "Cargo.toml"),
        "staticlib sources were not read correctly: {sources:?}"
    );
    assert!(
        sources.iter().any(|s| s == "chores.yml"),
        "staticlib `sources:` does not list chores.yml, so an edit to the \
         task's own cmds:/vars: leaves its fingerprint unchanged and chore \
         reports `task: staticlib is up to date` while shipping the previous \
         artefacts (#90). sources = {sources:?}"
    );
}

#[test]
fn task_sources_reads_a_listed_entry() {
    let text = "tasks:\n  staticlib:\n    sources:\n      - Cargo.toml\n      - chores.yml\n";
    assert_eq!(
        task_sources(text, "staticlib"),
        ["Cargo.toml", "chores.yml"]
    );
}
