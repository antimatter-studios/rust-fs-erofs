#!/usr/bin/env bash
#
# vm-setup.sh — the fs-linux-test-harness [setup] script. Runs as root
# INSIDE the VM, re-applied by the harness whenever this file changes.
#
# THE GUEST IS WHERE THE ORACLE TOOLS LIVE. Not the host. erofs-utils on
# a workstation is whatever that machine has, and for this filesystem
# that is worse than usual:
#
#   * Debian 12 packages 1.5 and Ubuntu 24.04 packages 1.7.1. Both
#     predate the variable block size this crate's writer emits and the
#     `Filesystem blocksize:` line tests/oracle_writer.rs reads out of
#     dump.erofs, so both answer wrongly rather than not at all.
#   * mkfs.erofs compiles its ZSTD support in ONLY when libzstd was
#     present at configure time, and most packaged builds are not built
#     that way -- `mkfs.erofs -V` there lists `lz4, lz4hc, lzma, deflate`
#     and `-zzstd` is refused outright.
#   * On a Mac there is no EROFS at all, packaged or otherwise.
#
# So it is built here, from source, at a pinned tag, with libzstd: one
# version, one platform, the same answers for everyone, and a developer's
# machine installs none of it.
#
#   erofs-utils  mkfs.erofs, fsck.erofs, dump.erofs — the oracle tools
#                (tests/support/src/oracle.rs) and the fixture builder's
#                formatter
#   attr, acl    setfattr/getfattr, setfacl/getfacl: the xattrs the
#                oracle and kernel paths read back
#   util-linux   losetup and mount: the kernel oracle's loop mounts,
#                which happen here and nowhere else
#   python3      test-disks/guest-build-images.sh generates the fixture
#                source tree with it, including the 16 KiB LCG file
#                tests/fixture_matrix.rs regenerates to compare against
#   erofs.ko     the real in-kernel EROFS driver, which is the only
#                second implementation of the MOUNT path that exists
#
# AND A RUST TOOLCHAIN, for `chore test:vm` — the whole suite compiled
# and run in here, which is how a macOS host runs a Linux test suite at
# all. It is pinned to the repository's rust-toolchain.toml, installed
# under /var/lib (the VM's own disk, which outlives a `vm:down`), and the
# build directory lives there too so the second run is incremental.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

REPO=/repo
RUST_ROOT=/var/lib/fs-erofs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"

# THE erofs-utils PIN. A tag, not a branch or a snapshot URL: the family
# pins every sibling, every box and every toolchain, and an oracle that
# moves on its own is an oracle whose verdict cannot be compared with
# yesterday's. Bump it deliberately, and expect to re-measure the tiers
# when you do.
#
# 1.9.1 is what this repository has always validated against; the
# README's compatibility claims are written about it, and
# tests/support/src/oracle.rs refuses a guest built from another one so
# the two cannot drift.
EROFS_UTILS_PIN=v1.9.1
EROFS_UTILS_REPO=https://git.kernel.org/pub/scm/linux/kernel/git/xiang/erofs-utils.git
EROFS_UTILS_SRC=/var/lib/fs-erofs-utils
EROFS_UTILS_PREFIX=/usr/local

apt-get update -qq
apt-get install -y -qq attr acl fdisk util-linux curl gcc libc6-dev pkg-config \
    python3 \
    git autoconf automake libtool make \
    uuid-dev libselinux1-dev liblz4-dev liblzma-dev libzstd-dev zlib1g-dev >/dev/null

# The loop driver for the kernel oracle's mounts, and the EROFS module
# itself. Loaded here rather than at first use so a guest image without
# either fails the provision -- which names this script -- instead of
# failing one test with a mount error nobody can place.
modprobe loop
modprobe erofs
grep -qw erofs /proc/filesystems || {
    echo "vm-setup: this guest kernel has no EROFS driver, so the kernel oracle" >&2
    echo "          cannot mount anything. $(uname -r)" >&2
    exit 1
}

# erofs-utils, at the pinned tag. Idempotent by the stamp, which holds
# the tag that was built: a changed pin rebuilds, an unchanged one costs
# a `cat`. The harness re-runs this whole script whenever it changes (it
# stamps the script's own sha256 in the guest), so bumping the pin above
# is all it takes for the next boot to rebuild.
stamp="$EROFS_UTILS_PREFIX/lib/erofs-utils.pin"
if [ "$(cat "$stamp" 2>/dev/null || true)" != "$EROFS_UTILS_PIN" ]; then
    echo "vm-setup: building erofs-utils $EROFS_UTILS_PIN"
    if [ ! -d "$EROFS_UTILS_SRC/.git" ]; then
        rm -rf "$EROFS_UTILS_SRC"
        git init --quiet "$EROFS_UTILS_SRC"
        git -C "$EROFS_UTILS_SRC" remote add origin "$EROFS_UTILS_REPO"
    fi
    git -C "$EROFS_UTILS_SRC" fetch --quiet --depth 1 origin "$EROFS_UTILS_PIN"
    git -C "$EROFS_UTILS_SRC" checkout --quiet FETCH_HEAD

    # --enable-fuse=no: erofsfuse is a third way to read an image and we
    # ask the kernel instead. libzstd/liblzma/liblz4 are all detected,
    # and their absence is what silently drops a codec -- so the build is
    # checked below rather than trusted.
    (cd "$EROFS_UTILS_SRC" && ./autogen.sh >/dev/null 2>&1 &&
        ./configure --enable-fuse=no >/dev/null &&
        make -j"$(nproc)" >/dev/null &&
        make install >/dev/null)
    # LAST, so an interrupted build is not mistaken for a finished one.
    printf '%s\n' "$EROFS_UTILS_PIN" > "$stamp"
fi

# WHAT WAS BUILT, CHECKED — not what was asked for. A configure that
# failed to find libzstd still produces a working mkfs.erofs; it just
# refuses `-zzstd`, and the ZSTD oracle tests would then be comparing
# this crate against nothing.
#
# THE CHECK IS AN IMAGE, NOT A VERSION BANNER. Which codecs a build has
# is a property of what configure found, and the banner's wording has
# changed between releases -- so each codec is asked to produce an image,
# which is the thing the tests need it to do. A codec the build lacks
# fails the provision here, naming itself, rather than failing one oracle
# test with a refusal nobody can place.
#
# AND THE TOOL ON PATH IS THE ONE THAT WAS JUST BUILT, established by
# COMMIT rather than by a version number.
#
# erofs-utils does not report its tag. Built from the v1.9.1 tree its
# banner reads
#
#   mkfs.erofs (erofs-utils) 1.9-gd3fbccba
#
# -- PACKAGE_VERSION is the SERIES, 1.9, with the commit appended by the
# build. So there is no `1.9.1` in that line to compare, and a floor
# parsed out of it is worse than no floor: `sort -V` ranks
# `1.9-gd3fbccba` BELOW `1.9.1`, so the check rejected a guest built from
# exactly the pin it was asked for. This file already says the banner's
# wording is not a contract; reading a release out of it was the same
# mistake one paragraph further down.
#
# The release is decided where it is actually pinned -- the tag this tree
# was fetched at -- so that is not re-derived here. What is worth
# checking is the thing the version floor was really guarding against: a
# PACKAGED mkfs.erofs shadowing the built one on PATH. Debian's 1.5 and
# Ubuntu's 1.7.1 answer that banner `mkfs.erofs 1.5`, with no commit at
# all, so comparing the commit catches it and says which binary won.
#
# v1.9.1 is an ANNOTATED tag: the tag object is 750113ac and it peels to
# commit d3fbccba, which is what a checkout of FETCH_HEAD lands on and
# what appears in the banner. Comparing against rev-parse HEAD rather
# than a written-down hash keeps this true when the pin moves.
built_commit="$(git -C "$EROFS_UTILS_SRC" rev-parse --short=8 HEAD)"
# sed, not head, on the tool: head exits after one line, the tool gets
# SIGPIPE writing its second, and pipefail turns that into a failed setup.
version="$(mkfs.erofs --version 2>&1 | sed -n 1p)"
case "$version" in
    *"g$built_commit"*) ;;
    *)
        echo "vm-setup: the mkfs.erofs on PATH is not the one built here." >&2
        echo "          banner:        ${version:-(nothing)}" >&2
        echo "          built from:    $EROFS_UTILS_PIN ($built_commit)" >&2
        echo "          resolved to:   $(command -v mkfs.erofs || echo 'not on PATH')" >&2
        echo "          A packaged erofs-utils earlier on PATH answers without a" >&2
        echo "          commit (Debian 12 'mkfs.erofs 1.5', Ubuntu 24.04 1.7.1) and" >&2
        echo "          knows neither the writer's block sizes nor dump.erofs's" >&2
        echo "          'Filesystem blocksize:' line, so the oracles would compare" >&2
        echo "          this crate against the wrong tool." >&2
        exit 1
        ;;
esac

probe="$(mktemp -d)"
mkdir -p "$probe/src"
# Compressible enough that every codec has something to do.
#
# NOT `yes ... | head -400`. head closes the pipe once it has its count,
# `yes` is killed by SIGPIPE, and `pipefail` turns that 141 into a failed
# provision -- with no message, because the failing command is the one
# that died. This file warns about precisely that trap a few paragraphs
# up, for `sed`; `yes` is the case where it is not a risk but a
# certainty, since the producer never ends on its own.
#
# printf repeats its format once per remaining argument, so `seq` carries
# the count and nothing is reading from a pipe that closes.
for i in 1 2 3 4 5 6 7 8; do
    # shellcheck disable=SC2046  # the word splitting is the repeat count
    printf 'the same line over and over\n%.0s' $(seq 400) > "$probe/src/f$i.txt"
done
for codec in lz4 lzma deflate zstd; do
    if ! mkfs.erofs -z"$codec" "$probe/out.img" "$probe/src" >/dev/null 2>"$probe/err"; then
        echo "vm-setup: this mkfs.erofs cannot write $codec:" >&2
        sed -n '1,5p' "$probe/err" >&2
        echo "          It was configured without that codec's library, and the oracle" >&2
        echo "          tests for it would compare this crate against nothing." >&2
        exit 1
    fi
    fsck.erofs "$probe/out.img" >/dev/null 2>&1 || {
        echo "vm-setup: fsck.erofs rejects the $codec image its own mkfs just wrote" >&2
        exit 1
    }
    rm -f "$probe/out.img"
done
rm -rf "$probe"

echo "vm-setup: $version writes lz4, lzma, deflate and zstd"
fsck.erofs --version 2>&1 | sed -n 1p
dump.erofs --version 2>&1 | sed -n 1p

# The toolchain the repository pins, and only that one: a guest that
# silently built with a different compiler than CI is a guest whose
# result means nothing.
toolchain="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$REPO/rust-toolchain.toml" | head -1)"
[ -n "$toolchain" ] || { echo "vm-setup: no channel in $REPO/rust-toolchain.toml" >&2; exit 1; }

mkdir -p "$RUST_ROOT"
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --no-modify-path --default-toolchain none >/dev/null
fi
"$CARGO_HOME/bin/rustup" toolchain install "$toolchain" \
    --component rustfmt --component clippy --profile minimal >/dev/null
"$CARGO_HOME/bin/rustup" default "$toolchain" >/dev/null
"$CARGO_HOME/bin/cargo" --version

echo "vm-setup: the oracle tools, the EROFS driver and the pinned toolchain are in the guest"
