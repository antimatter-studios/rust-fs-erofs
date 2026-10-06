#!/usr/bin/env bash
# A tier that ignored a test fails, however many others it ran (#129).
#
# A test marked `#[ignore]`, or one a selection opted out, leaves `N passed;
# 0 failed; 1 ignored` behind, and a floor that reads only the passed count
# stays green as long as N clears it. A test that decided not to run is not a
# test that passed. rust-fs-core's floor refuses that when asked
# (`../rust-fs-core/scripts/test-floor.sh --refuse-ignored`), and proves it in its own
# tests; this repository's part is asking. So every floor this repository
# runs -- in chores.yml and in the workflows -- must carry --refuse-ignored,
# and there must be floors to check.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1
pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }
printf 'skip-gate\n'
calls="$(grep -nE 'test-floor\.sh' chores.yml .github/workflows/*.yml 2>/dev/null \
    | grep -vE ':[0-9]+:\s*#' || true)"
count="$(printf '%s\n' "$calls" | grep -c . || true)"
if [ "${count:-0}" -ge 5 ]; then ok; else
    bad "found ${count:-0} floor calls; expected every cargo tier to end in one"
fi
missing="$(printf '%s\n' "$calls" | grep -v -- '--refuse-ignored' | grep . || true)"
if [ -z "$missing" ]; then ok; else
    bad "these floors would let an ignored test pass:"$'\n'"$missing"
fi
printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
