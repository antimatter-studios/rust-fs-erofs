#!/usr/bin/env bash
# package-cli.sh <version> <label> [target-dir]
#
# Package the built command-line tools as a release tarball in the current
# directory, check it, and print its file name on stdout.
#
#   <version>     the release version, without the leading `v`
#   <label>       the platform, e.g. darwin-arm64 or linux-x86_64
#   [target-dir]  where cargo put the release build (default: target/release)
#
# THE TARBALL IS THE CONTRACT with whatever installs it. Its layout is an
# install prefix, the same in every repository of the family, so an
# installer copies it as-is and needs to know nothing about which tools are
# in it:
#
#   bin/<repo>                          the multi-call binary, the real file
#   bin/<tool>...  -> <repo>            each dotted name, a relative symlink
#   share/man/man1/, share/man/man8/    man pages, one per name and subcommand
#   share/zsh/site-functions/_<name>
#   share/bash-completion/completions/<name>
#   share/fish/vendor_completions.d/<name>.fish
#   share/<repo>/CAVEATS                at most four lines an installer shows
#   LICENSE
#
# <repo> is the repository's name, from Cargo.toml's `repository`, and is
# the binary's own name. Cargo refuses a dot in a target name, so the dotted
# names exist only as the symlinks made here: no build-system name reaches a
# published artifact. The man pages and completions are written by the
# binary itself (`<repo> generate man|completions`), so they describe the
# flags the program actually takes.
#
# THEN IT CHECKS WHAT IT BUILT, because a tarball whose tools do not run is
# worse than no tarball: the failure would surface as a user's bug report
# rather than a red release. The dotted names are written down HERE, not
# read from the binary alone, so a binary that forgot one cannot agree with
# itself. Every file outside the layout above is refused; every name must
# be a relative symlink to bin/<repo>, answer --help, report `<tool>
# (<crate>) <version>` for this version, and have its man page and its three
# completions. On any failure nothing is printed on stdout and the tarball
# is removed. tests/scripts/test-package-cli.sh holds this script to all of
# that.
set -euo pipefail

version="${1:-}"
label="${2:-}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${3:-$root/target/release}"

# The dotted names this repository ships, and each one's manual section.
names=(mkfs.erofs:8 fs.erofs:1)
licences=(LICENSE)

die() { echo "package-cli: $*" >&2; exit 1; }

[ -n "$version" ] || die "usage: package-cli.sh <version> <label> [target-dir]"
[ -n "$label" ] || die "usage: package-cli.sh <version> <label> [target-dir]"

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
[ -n "$crate" ] || die "no package name in $root/Cargo.toml"
repo="$(sed -n 's/^repository = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
repo="${repo%/}"
repo="${repo##*/}"
[ -n "$repo" ] || die "no repository in $root/Cargo.toml"

tarball="$crate-$version-$label.tar.gz"
work="$(mktemp -d)"

# ON ANY FAILURE, NO TARBALL: not a partial one, and not one a previous run
# left under the same name, which a caller could otherwise take for this
# run's output.
cleanup() {
    local status=$?
    rm -rf "$work"
    [ "$status" -eq 0 ] || rm -f "$tarball"
    return "$status"
}
trap cleanup EXIT

built="$target_dir/$repo"
[ -x "$built" ] || die "no built $repo at $built (cargo build --release --locked --features cli --bin $repo)"

stage="$work/stage"
mkdir -p "$stage/bin" "$stage/share/$repo" "$work/unpacked"
cp "$built" "$stage/bin/$repo"
chmod 755 "$stage/bin/$repo"

want_names="$(printf '%s\n' "${names[@]%%:*}")"
got_names="$("$stage/bin/$repo" generate names)" || die "$repo generate names failed"
[ "$got_names" = "$want_names" ] \
    || die "$repo generate names lists [$(echo $got_names)], expected [$(echo $want_names)]"
for n in "${names[@]}"; do
    ln -s "$repo" "$stage/bin/${n%%:*}"
done
"$stage/bin/$repo" generate man "$stage/share" >/dev/null || die "$repo generate man failed"
"$stage/bin/$repo" generate completions "$stage/share" >/dev/null || die "$repo generate completions failed"
cp "$root/packaging/CAVEATS" "$stage/share/$repo/CAVEATS"
for f in "${licences[@]}"; do
    cp "$root/$f" "$stage/$f"
done

# COPYFILE_DISABLE keeps macOS tar from adding ._ AppleDouble members.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$stage" bin share "${licences[@]}"

tar -xzf "$tarball" -C "$work/unpacked"
u="$work/unpacked"

# Nothing outside the layout. Files and symlinks only: whether a tar lists
# the directories themselves varies by tar.
stray="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | grep -vE \
    "^(bin/[^/]+|share/man/man[18]/[^/]+\.[18]|share/zsh/site-functions/_[^/]+|share/bash-completion/completions/[^/]+|share/fish/vendor_completions\.d/[^/]+\.fish|share/$repo/CAVEATS|LICENSE)$" || true)"
[ -z "$stray" ] || die "$tarball holds files outside the install layout: $(echo $stray)"
bins="$(cd "$u/bin" && ls | LC_ALL=C sort | tr '\n' ' ')"
want_bins="$(printf '%s\n' "$repo" "${names[@]%%:*}" | LC_ALL=C sort | tr '\n' ' ')"
[ "$bins" = "$want_bins" ] || die "bin/ holds [$bins], expected [$want_bins]"

[ -f "$u/bin/$repo" ] && [ ! -L "$u/bin/$repo" ] || die "bin/$repo is not the real file"
for f in "${licences[@]}"; do
    cmp -s "$u/$f" "$root/$f" || die "$f is not the repository's"
done
caveats="$u/share/$repo/CAVEATS"
[ -s "$caveats" ] || die "share/$repo/CAVEATS is empty"
[ "$(wc -l <"$caveats" | tr -d ' ')" -le 4 ] || die "share/$repo/CAVEATS is more than four lines"

for n in "$repo:1" "${names[@]}"; do
    name="${n%%:*}"
    section="${n##*:}"
    exe="$u/bin/$name"
    if [ "$name" != "$repo" ]; then
        [ -L "$exe" ] || die "bin/$name is not a symlink"
        [ "$(readlink "$exe")" = "$repo" ] || die "bin/$name points at '$(readlink "$exe")', not '$repo'"
    fi
    [ -x "$exe" ] || die "bin/$name is not executable in $tarball"
    "$exe" --help >/dev/null || die "$name --help failed"
    reported="$("$exe" --version)" || die "$name --version failed"
    [ "$reported" = "$name ($crate) $version" ] \
        || die "$name --version says '$reported', expected '$name ($crate) $version'"
    [ -s "$u/share/man/man$section/$name.$section" ] || die "no man page share/man/man$section/$name.$section"
    [ -s "$u/share/zsh/site-functions/_$name" ] || die "no zsh completion for $name"
    [ -s "$u/share/bash-completion/completions/$name" ] || die "no bash completion for $name"
    [ -s "$u/share/fish/vendor_completions.d/$name.fish" ] || die "no fish completion for $name"
done

printf '%s\n' "$tarball"
