#!/usr/bin/env bash
# The release tarball is an install prefix -- bin/rust-fs-erofs, each dotted
# name a relative symlink to it, the man pages, the completions, the CAVEATS
# and the licence, nothing else -- and every name in it runs and identifies
# itself.
#
# This runs the real packaging script against stand-ins for the built
# multi-call binary in a sandbox: one that behaves, and one for each way a
# build can be wrong (missing, forgetting a name, --help failing, reporting
# a version other than the tag's, writing no man page). The `cli` CI job and
# the release workflow run the same script against the real binary, so the
# checks here are the checks a release makes.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PACKAGE="$ROOT/scripts/package-cli.sh"
REPO_NAME=rust-fs-erofs
pass=0
fail=0

ok()  { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$*"; }

mkdir -p "$ROOT/tmp"
sandbox="$(mktemp -d "$ROOT/tmp/package-cli.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[ "$crate" = "am-fs-erofs" ] && ok || bad "crate name read from Cargo.toml: '$crate'"

# A stand-in for the built binary, in $sandbox/<dir>/rust-fs-erofs.
#   VERSION  what --version reports          (default 9.9.9)
#   HELP     the exit status of --help       (default 0)
#   NAMES    what `generate names` prints    (default the two dotted names)
#   NO_MAN   a name to write no man page for
stub() {
    local dir="$sandbox/$1"
    mkdir -p "$dir"
    cat >"$dir/$REPO_NAME" <<STUB
#!/usr/bin/env bash
me="\$(basename "\$0")"
case "\$1" in
    --help) echo "Usage: \$me"; exit ${HELP:-0} ;;
    --version) echo "\$me ($crate) ${VERSION:-9.9.9}"; exit 0 ;;
    generate)
        case "\$2" in
            names) printf '%s\n' ${NAMES:-mkfs.erofs fs.erofs} ;;
            man)
                mkdir -p "\$3/man/man1" "\$3/man/man8"
                for n in mkfs.erofs:8 fs.erofs:1 $REPO_NAME:1; do
                    [ "\${n%%:*}" = "${NO_MAN:-}" ] && continue
                    echo '.TH X' > "\$3/man/man\${n##*:}/\${n%%:*}.\${n##*:}"
                done ;;
            completions)
                mkdir -p "\$3/zsh/site-functions" "\$3/bash-completion/completions" "\$3/fish/vendor_completions.d"
                for n in mkfs.erofs fs.erofs $REPO_NAME; do
                    echo "#compdef \$n" > "\$3/zsh/site-functions/_\$n"
                    echo "complete -F _\$n \$n" > "\$3/bash-completion/completions/\$n"
                    echo "complete -c \$n" > "\$3/fish/vendor_completions.d/\$n.fish"
                done ;;
        esac ;;
    *) exit 2 ;;
esac
STUB
    chmod +x "$dir/$REPO_NAME"
    printf '%s\n' "$dir"
}

# Runs the packaging script in a fresh output directory and prints the
# tarball's absolute path. STALE=<name> first leaves a file of that name
# there, as a previous run would have. The output directory is recorded in
# $sandbox/package-out for the leftover-tarball check.
package() {
    local out="$sandbox/out-$RANDOM$RANDOM" name status
    mkdir -p "$out"
    printf '%s\n' "$out" > "$sandbox/package-out"
    [ -z "${STALE:-}" ] || echo stale > "$out/$STALE"
    name="$(cd "$out" && bash "$PACKAGE" "$@" 2>"$sandbox/stderr")"
    status=$?
    if [ "$status" -ne 0 ]; then
        printf '%s' "$name"
        return "$status"
    fi
    printf '%s\n' "$out/$name"
}

[ -f "$PACKAGE" ] && ok || bad "scripts/package-cli.sh exists"

# --- A good build: the tarball, its name, and exactly its layout. ---------
good="$(stub good)"
if tarball="$(package 9.9.9 darwin-arm64 "$good")"; then
    ok
else
    bad "a good build packages: $(cat "$sandbox/stderr")"
    tarball=""
fi
case "$(basename "$tarball")" in
    "$crate-9.9.9-darwin-arm64.tar.gz") ok ;;
    *) bad "tarball is named <crate>-<version>-<label>.tar.gz, got '$tarball'" ;;
esac

# The content checks need the tarball. Without it they fail rather than
# fall silent, since a check that does not run reads like one that passed.
[ -f "$tarball" ] && ok || bad "the packaged tarball exists at '$tarball'"
if [ -f "$tarball" ]; then
    files="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | LC_ALL=C sort | tr '\n' ' ')"
    want="LICENSE bin/fs.erofs bin/mkfs.erofs bin/rust-fs-erofs"
    want="$want share/bash-completion/completions/fs.erofs share/bash-completion/completions/mkfs.erofs share/bash-completion/completions/rust-fs-erofs"
    want="$want share/fish/vendor_completions.d/fs.erofs.fish share/fish/vendor_completions.d/mkfs.erofs.fish share/fish/vendor_completions.d/rust-fs-erofs.fish"
    want="$want share/man/man1/fs.erofs.1 share/man/man1/rust-fs-erofs.1 share/man/man8/mkfs.erofs.8"
    want="$want share/rust-fs-erofs/CAVEATS"
    want="$want share/zsh/site-functions/_fs.erofs share/zsh/site-functions/_mkfs.erofs share/zsh/site-functions/_rust-fs-erofs "
    [ "$files" = "$want" ] && ok || bad "tarball holds exactly the install layout, got: $files"
    case "$files" in
        *mkfs_erofs*) bad "a cargo target name reached the tarball: $files" ;;
        *) ok ;;
    esac
    unpacked="$sandbox/unpacked"
    mkdir -p "$unpacked"
    tar -xzf "$tarball" -C "$unpacked"
    for name in mkfs.erofs fs.erofs; do
        [ -L "$unpacked/bin/$name" ] && [ "$(readlink "$unpacked/bin/$name")" = "$REPO_NAME" ] && ok \
            || bad "bin/$name is a relative symlink to $REPO_NAME"
    done
    [ -f "$unpacked/bin/$REPO_NAME" ] && [ ! -L "$unpacked/bin/$REPO_NAME" ] && ok \
        || bad "bin/$REPO_NAME is the real file"
    cmp -s "$unpacked/bin/$REPO_NAME" "$good/$REPO_NAME" && ok || bad "bin/$REPO_NAME is the built binary"
    cmp -s "$unpacked/share/$REPO_NAME/CAVEATS" "$ROOT/packaging/CAVEATS" && ok \
        || bad "share/$REPO_NAME/CAVEATS is packaging/CAVEATS"
    [ "$(wc -l <"$ROOT/packaging/CAVEATS" | tr -d ' ')" -le 4 ] && ok || bad "packaging/CAVEATS is at most four lines"
    grep -q 'brew unlink erofs-utils' "$ROOT/packaging/CAVEATS" && ok \
        || bad "packaging/CAVEATS gives the brew unlink line for erofs-utils' mkfs.erofs"
    cmp -s "$unpacked/LICENSE" "$ROOT/LICENSE" && ok || bad "LICENSE is the repository's"
fi

# --- Each way a build can be wrong is refused, with no tarball left. ------
refused() {
    local why="$1"; shift
    local stdout out_dir left
    if stdout="$(package "$@")"; then
        bad "$why is refused, but packaging succeeded: $stdout"
    else
        ok
        [ -z "$stdout" ] && ok || bad "$why leaves no tarball named on stdout: $stdout"
        out_dir="$(cat "$sandbox/package-out")"
        left="$(find "$out_dir" -maxdepth 1 -name '*.tar.gz')"
        [ -z "$left" ] && ok || bad "$why leaves no tarball behind, found: $left"
    fi
}

refused "a missing binary" 9.9.9 darwin-arm64 "$sandbox/nowhere"
refused "a binary that forgets a name" 9.9.9 darwin-arm64 "$(NAMES=mkfs.erofs stub forgets)"
refused "a binary whose --help fails" 9.9.9 darwin-arm64 "$(HELP=1 stub helpfails)"
refused "a binary reporting a version other than the tag's" 9.9.9 darwin-arm64 "$(VERSION=1.0.0 stub wrongver)"
refused "a binary that writes no man page for fs.erofs" 9.9.9 darwin-arm64 "$(NO_MAN=fs.erofs stub noman)"
refused "a missing label" 9.9.9 "" "$good"
refused "a missing version" "" darwin-arm64 "$good"
STALE="$crate-9.9.9-darwin-arm64.tar.gz" refused "a failure beside a previous run's tarball" 9.9.9 darwin-arm64 "$sandbox/nowhere"

printf 'package-cli: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
