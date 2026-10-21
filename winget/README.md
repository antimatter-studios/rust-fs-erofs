# winget manifests

The manifests that submit the Windows tools to
[winget-pkgs](https://github.com/microsoft/winget-pkgs) (#185), one
directory per release tag. `v<X.Y.Z>/` is the layout winget-pkgs expects
under `manifests/a/AntimatterStudios/rust-fs-erofs/<X.Y.Z>/`:

- `AntimatterStudios.rust-fs-erofs.yaml`: the version manifest.
- `AntimatterStudios.rust-fs-erofs.locale.en-US.yaml`: description,
  license and tags.
- `AntimatterStudios.rust-fs-erofs.installer.yaml`: the two release zips,
  `windows-x86_64` as `x64` and `windows-arm64` as `arm64`, each with its
  SHA-256.

## What a user gets

The installer is the release zip itself, `InstallerType: zip` with
`NestedInstallerType: portable`. winget unpacks it and puts one command
alias on PATH per name in the zip's `bin\`: `rust-fs-erofs`, plus every
dotted name `[package.metadata.package-cli]` in `Cargo.toml` declares.
Each dotted `.exe` is a byte-identical copy of `rust-fs-erofs.exe`, and
the tool dispatches on the name it was invoked by, `.exe` stripped.

`tests/winget_manifests.rs` holds every directory here to that: the
aliases are exactly those names, the URLs are this repository's release
assets for the directory's version, the hashes are SHA-256 hex, the
license is `Cargo.toml`'s, and the three files agree on the identifier,
version and manifest version.

## A new version

1. Tag the release. `release.yml` attaches
   `rust-fs-erofs-<X.Y.Z>-windows-{x86_64,arm64}.zip` and attests them.
2. Copy the newest `v<X.Y.Z>/` to the new tag. Update `PackageVersion`,
   `ReleaseDate`, the URLs and the hashes. The hashes are the release
   asset digests:
   `gh release view v<X.Y.Z> --json assets -q '.assets[]|[.name,.digest]|@tsv'`
3. Validate on Windows: `winget validate --manifest .\winget\v<X.Y.Z>\`
4. Copy the three files into a fork of winget-pkgs and open a pull
   request there.

The first submission is made by hand. Later versions are meant to be
submitted by `release.yml`. That needs an account that forks winget-pkgs
and a `WINGET_TOKEN` secret for it, and both are still the owner's
decision.
