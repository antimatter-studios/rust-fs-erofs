#!/usr/bin/env bash
#
# build-fixtures.sh [name...]   build the test-disks/*.img fixtures
#                               (`chore fixtures`); name some to rebuild
#                               only those, e.g. `build-fixtures.sh lzma zstd`
# build-fixtures.sh --check     exit 1 naming every fixture that is missing
#
# THE FIXTURE LIST lives here (IMAGES below), in
# test-disks/guest-build-images.sh, and in chores.yml's `fixtures`
# generates:. All three must name the same files.
#
# Every fixture here is written by `mkfs.erofs`, and the only erofs-utils
# this project trusts is the 1.9.1 built from source inside the
# fs-linux-test-harness VM (the sibling checkout at
# ../fs-linux-test-harness, moved to its pinned ref by `chore siblings`).
# So the work happens there: test-disks/guest-build-images.sh runs as
# root in the guest, writes finished images into the shared directory,
# and this script moves them into test-disks/ on the host.
#
# The VM comes down when this script exits (FLTH_KEEP_VM=1 keeps it up
# for a quicker next run).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

IMAGES="plain lz4 lz4hc lzma deflate zstd b1024 b512 bigpcluster ztailpacking chunked"

if [ "${1:-}" = "--check" ]; then
    gone=""
    for name in $IMAGES; do
        [ -f "$REPO/test-disks/erofs-$name.img" ] || gone="$gone erofs-$name.img"
    done
    if [ -n "$gone" ]; then
        echo "fixtures missing from test-disks/:$gone" >&2
        echo "build them with 'chore fixtures' — tests never skip on a missing fixture." >&2
        exit 1
    fi
    echo "fixtures: all $(echo $IMAGES | wc -w) present in test-disks/"
    exit 0
fi

targets="$*"
[ -n "$targets" ] || targets="$IMAGES"

HARNESS="$REPO/../fs-linux-test-harness"
VM="$HARNESS/scripts/vm.sh"

if [ ! -x "$VM" ]; then
    echo "build-fixtures: the harness is not checked out at $HARNESS." >&2
    echo "                Run 'chore siblings' first." >&2
    exit 1
fi

cd "$REPO"
# shellcheck source=/dev/null
. "$HARNESS/scripts/vm-session.sh"

share="$("$VM" share)"
out="$share/fixtures"
rm -rf "$out"
mkdir -p "$out"
cp test-disks/guest-build-images.sh "$share/guest-build-images.sh"

started=$(date +%s)
# shellcheck disable=SC2086  # the names are meant to split into words
"$VM" run "bash /share/guest-build-images.sh /share/fixtures $targets"

built=0
for img in "$out"/*.img; do
    [ -e "$img" ] || continue
    # Checked on the host rather than taken on the guest's word: every
    # image must carry the EROFS superblock magic (0xE0F5E1E2, little
    # endian, at byte 1024).
    base="$(basename "$img")"
    magic="$(od -An -tx1 -j1024 -N4 "$img" | tr -d ' \n')"
    if [ "$magic" != e2e1f5e0 ]; then
        echo "build-fixtures: $base has no EROFS superblock magic (got '$magic')" >&2
        exit 1
    fi
    cp --sparse=always "$img" "test-disks/$base.partial"
    mv -f "test-disks/$base.partial" "test-disks/$base"
    rm -f "$img"
    built=$((built + 1))
done
[ "$built" -gt 0 ] || { echo "build-fixtures: the guest produced no images" >&2; exit 1; }
echo "build-fixtures: $built image(s) in test-disks/ ($(( $(date +%s) - started ))s in the VM)"
