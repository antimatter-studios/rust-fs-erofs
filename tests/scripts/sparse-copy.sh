#!/usr/bin/env bash
# Tests for test-disks/sparse-copy.sh, the copy build-fixtures.sh uses to move
# each finished image out of the harness's shared directory into test-disks/.
#
# It used to be `cp --sparse=always`, which only GNU cp has: macOS cp stops at
# the option, so no fixture could be built on a Mac (#155). CI never saw it,
# because CI's cp is GNU. So the copy runs here twice: once with whatever cp
# this host has, and once with a stand-in for macOS cp first on PATH -- one
# that refuses every long option, has no --version, and writes every byte
# (BSD cp does not keep holes). Both must produce the same bytes, the same
# length and a file that is still mostly holes.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
COPY="$ROOT/test-disks/sparse-copy.sh"

pass=0; fail=0
ok()  { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

printf 'sparse-copy\n'

mkdir -p "$ROOT/tmp"
work="$(mktemp -d "$ROOT/tmp/sparse-copy.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# The stand-in for macOS cp. `cp: illegal option -- -` and status 64 is what
# BSD cp prints for `--sparse=always`.
mkdir -p "$work/bsd-bin"
cat > "$work/bsd-bin/cp" <<'EOF'
#!/bin/sh
for a in "$@"; do
    case "$a" in
        --*) echo "cp: illegal option -- -" >&2; exit 64 ;;
    esac
done
[ "$#" -eq 2 ] || { echo "usage: cp source_file target_file" >&2; exit 64; }
cat "$1" > "$2"
EOF
chmod +x "$work/bsd-bin/cp"

# 8 MiB with data at the start, in the middle and nowhere at the end, so a
# copy that drops a trailing hole comes out short. Allocated size is read with
# `du -k`, which both GNU and BSD du answer the same way.
src="$work/src.img"
printf 'EROFS-head' > "$src"
printf 'middle' | dd of="$src" bs=1 seek=3145728 conv=notrunc 2>/dev/null
truncate -s 8M "$src" 2>/dev/null || dd if=/dev/null of="$src" bs=1 seek=8388608 2>/dev/null
src_kib="$(du -k "$src" | cut -f1)"

# The control: if this filesystem cannot hold a hole, nothing below can tell a
# sparse copy from a dense one, so that is a failure, not a pass.
if [ "$src_kib" -lt 1024 ]; then ok; else
    bad "the source is not sparse on $work (${src_kib} KiB allocated); run this where the filesystem keeps holes"
fi

check_copy() { # LABEL DST
    local label="$1" dst="$2"
    if cmp -s "$src" "$dst"; then ok; else bad "$label: the copy's bytes differ from the source"; fi
    if [ "$(wc -c < "$dst" | tr -d ' ')" = 8388608 ]; then ok; else
        bad "$label: the copy is $(wc -c < "$dst" | tr -d ' ') bytes, not 8388608 (a trailing hole was dropped)"
    fi
    local kib; kib="$(du -k "$dst" | cut -f1)"
    if [ "$kib" -lt 1024 ]; then ok; else bad "$label: the copy is dense (${kib} KiB allocated for 8 MiB of mostly holes)"; fi
}

if "$COPY" "$src" "$work/host.img" 2>"$work/host.err"; then
    ok; check_copy "host cp" "$work/host.img"
else
    bad "with this host's cp, sparse-copy.sh failed: $(head -1 "$work/host.err")"
fi

if PATH="$work/bsd-bin:$PATH" "$COPY" "$src" "$work/bsd.img" 2>"$work/bsd.err"; then
    ok; check_copy "macOS-style cp" "$work/bsd.img"
else
    bad "with a macOS-style cp first on PATH, sparse-copy.sh failed: $(head -1 "$work/bsd.err")"
fi

# A missing source is a failure, whichever cp is in use -- a copy that
# "succeeds" at nothing would leave build-fixtures.sh moving an empty file.
if PATH="$work/bsd-bin:$PATH" "$COPY" "$work/absent.img" "$work/absent-out.img" 2>/dev/null; then
    bad "copying a missing source must fail"
else
    ok
fi

if "$COPY" "$src" 2>/dev/null; then bad "one argument must be a usage error"; else ok; fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
