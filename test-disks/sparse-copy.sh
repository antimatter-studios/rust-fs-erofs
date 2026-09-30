#!/usr/bin/env bash
#
# sparse-copy.sh SRC DST   copy SRC to DST, keeping its holes
#
# The fixtures are mostly holes, and build-fixtures.sh copies each one out
# of the harness's shared directory into test-disks/.
#
# WHY NOT `cp --sparse=always` ALONE: only GNU cp has it. macOS cp stops at
# the option ("cp: illegal option -- -"), so no fixture could be built on a
# Mac (#155), and CI, whose cp is GNU, never noticed. GNU cp is still used
# where it exists; anywhere else the copy is `dd conv=sparse`, which GNU and
# BSD dd both have: an all-zero block is seeked over rather than written.
#
# A copy whose last block is a hole ends with a seek, and a seek past the end
# does not by itself set a file's length. GNU dd fixes the length up; rather
# than trust every dd to, the length is set explicitly and then checked. tests/scripts/sparse-copy.sh runs
# both paths, the second with a stand-in for macOS cp first on PATH.
set -euo pipefail

[ "$#" -eq 2 ] || { echo "usage: sparse-copy.sh SRC DST" >&2; exit 2; }
src="$1"; dst="$2"

[ -f "$src" ] || { echo "sparse-copy: $src is not a file" >&2; exit 1; }
size="$(wc -c < "$src" | tr -d ' ')"

if cp --version 2>/dev/null | grep -q 'GNU coreutils'; then
    cp --sparse=always "$src" "$dst"
else
    dd if="$src" of="$dst" bs=65536 conv=sparse 2>/dev/null
    # Without conv=notrunc, dd truncates the output at the seek offset: here
    # that is the source's length, which extends a copy that ended in a hole
    # and cannot cut one that did not.
    dd if=/dev/null of="$dst" bs=1 seek="$size" 2>/dev/null
fi

got="$(wc -c < "$dst" | tr -d ' ')"
if [ "$got" != "$size" ]; then
    echo "sparse-copy: $dst is $got bytes, $src is $size" >&2
    exit 1
fi
