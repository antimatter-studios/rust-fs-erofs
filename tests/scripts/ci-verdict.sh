#!/usr/bin/env bash
# Truth table for scripts/ci-verdict.sh, and the workflow wiring that makes a
# pull request run every leg.
#
# jq is required, not optional: without it the verdict cannot be computed, so
# this test fails rather than passing on a runner that lacks it.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

command -v jq >/dev/null || { echo "ci-verdict: jq is required (apt-get install jq)"; exit 1; }

# needs JSON: every job with one result, then the named jobs overridden.
JOBS="changes unit benchmark fixtures test test-arm64 suite-in-vm cli cli-windows semver"
needs() {
  local over="$1" res="$2" out="{" sep="" j r
  for j in $JOBS; do
    r=success
    case " $over " in *" $j "*) r="$res" ;; esac
    out="$out$sep\"$j\":{\"result\":\"$r\"}"; sep=","
  done
  echo "$out}"
}
HEAVY_JOBS="$(sed -n 's/^  HEAVY_JOBS: *"\(.*\)"$/\1/p' .github/workflows/ci.yml)"
[ -n "$HEAVY_JOBS" ] && ok || bad "ci.yml declares HEAVY_JOBS in its top-level env"
verdict() { # want code over res [heavy]
  local want="$1" code="$2" over="$3" res="$4" heavy="${5:-true}" got
  needs "$over" "$res" | CODE="$code" HEAVY="$heavy" HEAVY_JOBS="$HEAVY_JOBS" \
    bash scripts/ci-verdict.sh >/dev/null 2>&1
  got=$?
  if { [ "$want" = green ] && [ $got -eq 0 ]; } || { [ "$want" = red ] && [ $got -ne 0 ]; }; then ok
  else bad "code=$code heavy=$heavy [$over]=$res: wanted $want, exit $got"; fi
}

verdict green true "" success
for j in $JOBS; do
  verdict red true  "$j" skipped        # code changed: every leg runs
  verdict red true  "$j" failure
  verdict red true  "$j" cancelled
  verdict red false "$j" failure        # documentation only still fails a failure
done
verdict green false "benchmark fixtures test test-arm64 suite-in-vm" skipped   # documentation only

# THE TOP OF A STACK RUNS EVERYTHING, A LOWER LAYER THE FAST CHECKS
# (owner-approved trial, 2026-11-20). With HEAVY=false the heavy jobs'
# skips are accepted -- all of them, or any of them -- and nothing else is.
verdict green true "$HEAVY_JOBS" skipped false        # a lower layer: heavy jobs skipped by design
verdict red   true "$HEAVY_JOBS" skipped true         # the top skipping them is a failure
for j in $JOBS; do
  verdict red true "$j" failure false                 # a job that ran and failed still fails
  verdict red true "$j" cancelled false
  case " $HEAVY_JOBS " in
    *" $j "*) verdict green true "$j" skipped false ;;
    *) verdict red true "$j" skipped false ;;         # a fast job's skip is never by design
  esac
done
for j in $HEAVY_JOBS; do
  case " $JOBS " in *" $j "*) ok ;; *) bad "HEAVY_JOBS names $j, which ci-ok does not need" ;; esac
done
needs "" success | CODE=true HEAVY=maybe HEAVY_JOBS="$HEAVY_JOBS" bash scripts/ci-verdict.sh >/dev/null 2>&1 \
  && bad "HEAVY other than true or false is refused" || ok
needs "fixtures" skipped | CODE=true HEAVY=false HEAVY_JOBS="" bash scripts/ci-verdict.sh >/dev/null 2>&1 \
  && bad "HEAVY=false with no HEAVY_JOBS is refused" || ok
verdict red   ""    "" success                                                  # no CODE, no verdict
needs "" success | CODE=true bash scripts/ci-verdict.sh >/dev/null 2>&1 && bad "no HEAVY, no verdict" || ok
verdict red   bogus "" success
echo '{}' | CODE=true bash scripts/ci-verdict.sh extra >/dev/null 2>&1 && bad "an argument is refused" || ok

# The workflow: ci-ok judges with the script, the unit job runs this test,
# and no job is gated on the event itself: which legs a pull request runs is
# decided by scripts/ci-heavy.sh alone, and the merge queue runs them all.
wf=.github/workflows/ci.yml
grep -q 'bash scripts/ci-verdict.sh' "$wf" && ok || bad "ci-ok judges with ci-verdict.sh"
grep -q 'bash tests/scripts/ci-verdict.sh' "$wf" && ok || bad "the unit job runs this test"
grep -q '^  workflow_dispatch:' "$wf" && ok || bad "ci.yml has a workflow_dispatch trigger"
grep -q '^  pull_request:' "$wf" && ok || bad "ci.yml runs on pull_request"
n="$(grep -cE "^\s+if:.*(profile|github\.event_name)" "$wf")"
[ "$n" -eq 0 ] && ok || bad "no job or step is gated on a profile or the event (found $n)"
[ ! -e scripts/ci-profile.sh ] && ok || bad "scripts/ci-profile.sh is gone, so nothing can choose a quick profile"

[ "$fail" -eq 0 ] || { printf 'ci-verdict: %d passed, %d failed\n' "$pass" "$fail"; exit 1; }
echo "ci-verdict.sh: all checks passed"
