#!/usr/bin/env bash
# A test that reads a whole disk image holds it in memory, once for every test
# running, and a comparison holds two copies (#302). tests/support/src/images.rs
# compares, snapshots and hashes in bounded memory, and `read_small_file`
# refuses anything over 1 MiB.
#
# This fails on any `fs::read` of a whole file in the tests beyond the ones
# listed below, each with its reason.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

# `fs::read(`, not `fs::read_dir`, `read_link` or `read_to_string`.
pattern='(^|[^[:alnum:]_])fs::read[[:space:]]*\('

# THE SCAN SEES WHAT IT IS FOR, so a pattern that matches nothing cannot pass.
printf '%s\n' 'let bytes = std::fs::read(&image);' | grep -qE "$pattern" ||
    fail "the scan does not recognise a whole-file read"
if printf '%s\n' 'for e in std::fs::read_dir(&dir) {' | grep -qE "$pattern"; then
    fail "the scan takes a directory listing for a whole-file read"
fi

# path, reads allowed there, and why.
allowed() {
    case "$1" in
        # An image built from a node tree already in memory, handed to the
        # primitive walkers as a slice. To be bounded under #302.
        tests/common/mod.rs) echo 1 ;;
        # The guest's captured stdout and stderr: command output, not an image.
        tests/support/src/oracle.rs) echo 1 ;;
        *) echo 0 ;;
    esac
}

scanned=0
while IFS= read -r source; do
    scanned=$((scanned + 1))
    rel="${source#"$REPO/"}"
    count="$(grep -cE "$pattern" "$source" || true)"
    want="$(allowed "$rel")"
    [ "$count" -eq "$want" ] || {
        fail "$rel has $count whole-file read(s), $want allowed: use tests/support/src/images.rs"
        grep -nE "$pattern" "$source" >&2 || true
    }
done < <(find "$REPO/tests" -name '*.rs' -type f | sort)
[ "$scanned" -gt 20 ] || fail "only $scanned test sources scanned"

[ "$fails" -eq 0 ] || exit 1
echo "PASS  no test reads a disk image whole ($scanned sources)"
