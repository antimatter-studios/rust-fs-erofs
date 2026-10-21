//! The winget manifests describe the release zips as they are (#185).
//!
//! `winget/v<X.Y.Z>/` holds what is submitted to microsoft/winget-pkgs
//! for that tag: a version manifest, a default-locale manifest and an
//! installer manifest. Nothing in CI installs them, and winget-pkgs'
//! own validation only sees them once someone opens a pull request
//! there, so a manifest naming a command the zip does not hold, or a URL
//! the release does not serve, would be found by a user.
//!
//! The installer is the release zip itself, unpacked as portable
//! commands: one alias per `bin\<name>.exe`, which is `rust-fs-erofs`
//! plus every dotted name `[package.metadata.package-cli]` declares. This
//! file holds every manifest directory to that, to the release asset
//! names release-cli gives the two Windows legs, and to `Cargo.toml`'s
//! license.
//!
//! The manifests are PARSED rather than scanned, so a comment cannot
//! satisfy a check meant for a field.

use saphyr::{LoadableYamlNode, Yaml};
use std::collections::BTreeSet;
use std::path::Path;

const IDENTIFIER: &str = "AntimatterStudios.rust-fs-erofs";

/// winget's architecture for each Windows leg of core's release-cli.
const ARCHITECTURES: &[(&str, &str)] = &[("x64", "windows-x86_64"), ("arm64", "windows-arm64")];

/// What the manifests are checked against, read from `Cargo.toml`.
struct Package {
    name: String,
    license: String,
    repository: String,
    /// The binary and every dotted name the zip holds beside it.
    commands: BTreeSet<String>,
}

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The value of a `key = "value"` line of `[package]`.
fn cargo_field(cargo: &str, key: &str) -> String {
    cargo
        .lines()
        .find_map(|l| l.strip_prefix(key)?.trim_start().strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .unwrap_or_else(|| panic!("Cargo.toml has no {key}"))
}

fn package() -> Package {
    let cargo = read("Cargo.toml");
    let name = cargo_field(&cargo, "name");
    // `names = { "<name>" = <section>, ... }`: the quoted keys.
    let names = cargo
        .split("[package.metadata.package-cli]")
        .nth(1)
        .expect("Cargo.toml has [package.metadata.package-cli]")
        .lines()
        .find_map(|l| l.trim().strip_prefix("names"))
        .expect("[package.metadata.package-cli] has names");
    let mut commands: BTreeSet<String> = names
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    assert!(!commands.is_empty(), "package-cli names no command");
    commands.insert(name.clone());
    Package {
        license: cargo_field(&cargo, "license"),
        repository: cargo_field(&cargo, "repository"),
        name,
        commands,
    }
}

fn load(text: &str) -> Yaml<'static> {
    let mut docs = Yaml::load_from_str(text).expect("the manifest parses as YAML");
    assert_eq!(docs.len(), 1, "one YAML document");
    docs.remove(0)
}

fn field<'a>(yaml: &'a Yaml<'static>, key: &str) -> Option<&'a str> {
    yaml.as_mapping_get(key).and_then(Yaml::as_str)
}

fn list<'a>(yaml: &'a Yaml<'static>, key: &str) -> &'a [Yaml<'static>] {
    yaml.as_mapping_get(key)
        .and_then(Yaml::as_sequence)
        .map_or(&[], Vec::as_slice)
}

/// Everything wrong with the manifests of `tag`, given the text of the
/// version, default-locale and installer manifests; empty when nothing is.
fn gaps(pkg: &Package, tag: &str, version: &str, locale: &str, installer: &str) -> Vec<String> {
    let mut gaps = Vec::new();
    let Some(want_version) = tag.strip_prefix('v') else {
        return vec![format!("directory {tag} is not v<X.Y.Z>")];
    };
    let docs = [
        ("version", load(version)),
        ("defaultLocale", load(locale)),
        ("installer", load(installer)),
    ];
    let manifest_version = field(&docs[0].1, "ManifestVersion");
    for (kind, doc) in &docs {
        let mut expect = |key: &str, want: Option<&str>| {
            if field(doc, key) != want {
                gaps.push(format!(
                    "{kind} manifest has {key} {:?}, not {want:?}",
                    field(doc, key)
                ));
            }
        };
        expect("ManifestType", Some(kind));
        expect("PackageIdentifier", Some(IDENTIFIER));
        expect("PackageVersion", Some(want_version));
        expect("ManifestVersion", manifest_version);
    }
    let [(_, version), (_, locale), (_, installer)] = &docs;
    if field(version, "DefaultLocale") != field(locale, "PackageLocale") {
        gaps.push("the version manifest's DefaultLocale is not the locale manifest's".into());
    }
    if field(locale, "License") != Some(pkg.license.as_str()) {
        gaps.push(format!(
            "locale manifest has License {:?}, not Cargo.toml's {:?}",
            field(locale, "License"),
            pkg.license
        ));
    }

    // THE COMMANDS: the zip unpacked as portables, one alias per binary.
    if field(installer, "InstallerType") != Some("zip")
        || field(installer, "NestedInstallerType") != Some("portable")
    {
        gaps.push("the installer is not the zip, unpacked as portable commands".into());
    }
    let mut aliases = BTreeSet::new();
    for file in list(installer, "NestedInstallerFiles") {
        let (path, alias) = (
            field(file, "RelativeFilePath"),
            field(file, "PortableCommandAlias"),
        );
        match (path, alias) {
            (Some(p), Some(a)) if p == format!("bin\\{a}.exe") => {
                aliases.insert(a.to_owned());
            }
            _ => gaps.push(format!(
                "nested file {path:?} is not bin\\<alias>.exe for alias {alias:?}"
            )),
        }
    }
    if aliases != pkg.commands {
        gaps.push(format!(
            "the aliases are {aliases:?}, not the zip's commands {:?}",
            pkg.commands
        ));
    }
    let commands: BTreeSet<String> = list(installer, "Commands")
        .iter()
        .filter_map(Yaml::as_str)
        .map(str::to_owned)
        .collect();
    if commands != pkg.commands {
        gaps.push(format!("Commands are {commands:?}, not {:?}", pkg.commands));
    }

    // THE DOWNLOADS: this tag's release zips, one per Windows leg.
    let installers = list(installer, "Installers");
    let arches: Vec<_> = installers
        .iter()
        .filter_map(|i| field(i, "Architecture"))
        .collect();
    let want_arches: Vec<_> = ARCHITECTURES.iter().map(|(a, _)| *a).collect();
    if arches.len() != installers.len() || {
        let mut a = arches.clone();
        a.sort_unstable();
        let mut w = want_arches.clone();
        w.sort_unstable();
        a != w
    } {
        gaps.push(format!(
            "installers are for {arches:?}, not {want_arches:?}"
        ));
    }
    for entry in installers {
        let arch = field(entry, "Architecture").unwrap_or("?");
        if let Some((_, label)) = ARCHITECTURES.iter().find(|(a, _)| *a == arch) {
            let url = format!(
                "{}/releases/download/{tag}/{}-{want_version}-{label}.zip",
                pkg.repository, pkg.name
            );
            if field(entry, "InstallerUrl") != Some(url.as_str()) {
                gaps.push(format!(
                    "{arch} InstallerUrl is {:?}, not {url:?}",
                    field(entry, "InstallerUrl")
                ));
            }
        }
        let sha = field(entry, "InstallerSha256").unwrap_or("");
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            gaps.push(format!("{arch} InstallerSha256 {sha:?} is not a SHA-256"));
        }
    }
    gaps
}

/// The manifest directories under winget/, by tag.
fn tags() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("winget");
    let mut tags: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read winget/: {e}"))
        .map(|e| e.expect("winget/ entry"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    tags.sort();
    tags
}

fn manifests(tag: &str) -> [String; 3] {
    ["", ".locale.en-US", ".installer"]
        .map(|kind| read(&format!("winget/{tag}/{IDENTIFIER}{kind}.yaml")))
}

/// Every winget/ directory describes its tag's release zips as the
/// commands the zip holds.
#[test]
fn the_winget_manifests_describe_the_release_zips() {
    let pkg = package();
    let tags = tags();
    assert!(!tags.is_empty(), "winget/ has no manifest directory");
    for tag in &tags {
        let [version, locale, installer] = manifests(tag);
        let gaps = gaps(&pkg, tag, &version, &locale, &installer);
        assert!(gaps.is_empty(), "winget/{tag}: {gaps:#?}");
    }
}

/// A check that cannot fail is no check: each way a manifest can drift
/// from the zip is refused.
#[test]
fn the_winget_checks_recognise_the_shapes_they_refuse() {
    let pkg = package();
    let tag = tags().pop().expect("winget/ has a manifest directory");
    let [version, locale, installer] = manifests(&tag);
    assert_eq!(
        gaps(&pkg, &tag, &version, &locale, &installer),
        Vec::<String>::new()
    );
    let some_command = pkg
        .commands
        .iter()
        .find(|c| **c != pkg.name)
        .expect("a dotted name");
    let expect = |version: &str, locale: &str, installer: &str, why: &str| {
        assert!(
            !gaps(&pkg, &tag, version, locale, installer).is_empty(),
            "not refused: {why}"
        );
    };
    let v = tag.trim_start_matches('v');
    expect(
        &version.replace(&format!("PackageVersion: {v}"), "PackageVersion: 0.0.1"),
        &locale,
        &installer,
        "a version manifest for another version",
    );
    expect(
        &version,
        &locale.replace("License: ", "License: GPL-"),
        &installer,
        "another license",
    );
    expect(
        &version,
        &locale,
        &installer.replace(
            &format!("PortableCommandAlias: {some_command}"),
            "PortableCommandAlias: other",
        ),
        "an alias for no binary",
    );
    expect(
        &version,
        &locale,
        &installer.replace(&format!("  - {some_command}\n"), ""),
        "a command missing from Commands",
    );
    expect(
        &version,
        &locale,
        &installer.replace("windows-arm64.zip", "windows-x86_64.zip"),
        "the arm64 installer downloading the x64 zip",
    );
    expect(
        &version,
        &locale,
        &installer.replace(&format!("download/{tag}/"), "download/v0.0.1/"),
        "a URL for another release",
    );
    expect(
        &version,
        &locale,
        &installer.replace("NestedInstallerType: portable", "NestedInstallerType: exe"),
        "not portable",
    );
    expect(
        &version,
        &locale,
        &installer.replace("Architecture: arm64", "Architecture: x86"),
        "a leg release-cli does not build",
    );
    expect(
        &version,
        &locale,
        &installer.replace("InstallerSha256: ", "InstallerSha256: Z"),
        "a hash that is not SHA-256",
    );
    expect(
        &version,
        &locale,
        &installer.replace("ManifestVersion: ", "ManifestVersion: 9"),
        "a manifest version that disagrees",
    );
}
