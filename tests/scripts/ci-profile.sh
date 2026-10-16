#!/usr/bin/env bash
# Truth table for scripts/ci-profile.sh: which events run the quick profile,
# and which job results ci-ok accepts under each profile.
#
# jq is required, not optional: without it the verdict cannot be computed, so
# this test fails rather than passing on a runner that lacks it.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

command -v jq >/dev/null || { echo "ci-profile: jq is required (apt-get install jq)"; exit 1; }

# Events.
for ev in merge_group push workflow_dispatch schedule; do
  got="$(bash scripts/ci-profile.sh profile "$ev")"
  [ "$got" = full ] && ok || bad "$ev runs full (got $got)"
done
got="$(bash scripts/ci-profile.sh profile pull_request)"
[ "$got" = quick ] && ok || bad "pull_request runs quick (got $got)"
bash scripts/ci-profile.sh profile >/dev/null 2>&1 && bad "a missing event is refused" || ok

# needs JSON: every job with one result, then one job overridden.
JOBS="changes unit fixtures test test-arm64 suite-in-vm cli cli-windows semver"
needs() {
  local over="$1" res="$2" out="{" sep="" j r
  for j in $JOBS; do
    r=success
    case " $over " in *" $j "*) r="$res" ;; esac
    out="$out$sep\"$j\":{\"result\":\"$r\"}"; sep=","
  done
  echo "$out}"
}
verdict() { # want code profile over res
  local want="$1" code="$2" profile="$3" over="$4" res="$5" got
  needs "$over" "$res" | CODE="$code" PROFILE="$profile" bash scripts/ci-profile.sh verdict >/dev/null 2>&1
  got=$?
  if { [ "$want" = green ] && [ $got -eq 0 ]; } || { [ "$want" = red ] && [ $got -ne 0 ]; }; then ok
  else bad "code=$code profile=$profile [$over]=$res: wanted $want, exit $got"; fi
}

FULL_ONLY="fixtures test test-arm64 suite-in-vm cli cli-windows"
QUICK="changes unit semver"

verdict green true full "" success
verdict green true quick "" success
for j in $FULL_ONLY; do
  verdict green true quick "$j" skipped      # quick skips every full-only leg
  verdict red   true full  "$j" skipped      # full never accepts a skipped leg
  verdict red   true quick "$j" failure
  verdict red   true quick "$j" cancelled
  verdict red   true full  "$j" failure
done
verdict green true quick "$FULL_ONLY" skipped
verdict red   true full  "$FULL_ONLY" skipped
for j in $QUICK; do
  verdict red true quick "$j" skipped        # the quick tier always runs
  verdict red true full  "$j" skipped
  verdict red true quick "$j" failure
done
verdict green false full "fixtures test test-arm64 suite-in-vm" skipped   # documentation only
verdict red   false full "unit" failure
verdict red   true ""    "" success                                        # no profile, no verdict
verdict red   true bogus "" success

# The workflow uses the script for both decisions, and can be run by hand.
grep -q 'scripts/ci-profile.sh profile' .github/workflows/ci.yml && ok || bad "ci.yml classifies the event with ci-profile.sh"
grep -q 'scripts/ci-profile.sh verdict' .github/workflows/ci.yml && ok || bad "ci-ok judges with ci-profile.sh"
grep -q '^  workflow_dispatch:' .github/workflows/ci.yml && ok || bad "ci.yml has a workflow_dispatch trigger"
grep -q 'bash tests/scripts/ci-profile.sh' .github/workflows/ci.yml && ok || bad "a quick-tier job runs this test"
n="$(grep -c "needs.changes.outputs.profile == 'full'" .github/workflows/ci.yml)"
[ "$n" -ge 3 ] && ok || bad "fixtures, cli and cli-windows are gated on the full profile (found $n)"

[ "$fail" -eq 0 ] || { printf 'ci-profile: %d passed, %d failed\n' "$pass" "$fail"; exit 1; }
echo "ci-profile.sh: all checks passed"
