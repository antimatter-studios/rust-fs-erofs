#!/usr/bin/env bash
# What scripts/ci-cancel-below.sh does to the layer below a new top.
#
# `gh` is a stub that logs every call to $LOG and answers from fixtures:
# the pull request on the base branch, its runs (filtered by the script's own
# --jq, run through the real jq), each run's status and jobs, and failures on
# demand (STUB_FAIL: cancel or rerun). Nothing reaches GitHub.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1
command -v jq >/dev/null || { echo "ci-cancel-below: jq is required"; exit 1; }

pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

stub="$(mktemp -d "${TMPDIR:-/tmp}/ci-cancel-below.XXXXXX")"
trap 'rm -rf "$stub"' EXIT
LOG="$stub/log"
cat > "$stub/gh" <<'GH'
#!/usr/bin/env bash
echo "$*" >> "$LOG"
jqexpr=""; prev=""
for a in "$@"; do [ "$prev" = --jq ] && jqexpr="$a"; prev="$a"; done
answer() { if [ -n "$jqexpr" ]; then jq -r "$jqexpr"; else cat; fi; }
case "$1 $2" in
  "pr list") echo "${STUB_PR:-[]}" | answer ;;
  "run list") echo "${STUB_RUNS:-[]}" | answer ;;
  "run cancel") [ "${STUB_FAIL:-}" = cancel ] && exit 1; exit 0 ;;
  "run view")
    case " $* " in
      *" status "*) echo '{"status":"completed"}' | answer ;;
      *) echo "${STUB_JOBS:-{\"jobs\":[]}}" | answer ;;
    esac ;;
  "run rerun") exit 0 ;;
  "api -X") [ "${STUB_FAIL:-}" = rerun ] && exit 1; exit 0 ;;
  *) exit 4 ;;
esac
GH
chmod +x "$stub/gh"

PR='[{"number":41,"headRefOid":"aaa"}]'
RUNS='[
 {"databaseId":101,"status":"in_progress","headSha":"aaa","event":"pull_request"},
 {"databaseId":102,"status":"queued","headSha":"aaa","event":"pull_request"},
 {"databaseId":103,"status":"completed","headSha":"aaa","event":"pull_request"},
 {"databaseId":104,"status":"in_progress","headSha":"old","event":"pull_request"},
 {"databaseId":105,"status":"in_progress","headSha":"aaa","event":"merge_group"},
 {"databaseId":106,"status":"in_progress","headSha":"aaa","event":"push"}]'
JOBS='{"jobs":[{"name":"changes (does this need the full pipeline?)","databaseId":9001},
               {"name":"fixtures (harness VM, x86_64 KVM)","databaseId":9002}]}'

run() { # event top base [fail]
  : > "$LOG"
  LOG="$LOG" PATH="$stub:$PATH" EVENT="$1" TOP="$2" BASE_REF="$3" DEFAULT_BRANCH=main REPO=o/r \
    WAIT_SECONDS=5 STUB_PR="${STUB_PR_OVERRIDE:-$PR}" STUB_RUNS="$RUNS" STUB_JOBS="$JOBS" \
    STUB_FAIL="${4:-}" bash scripts/ci-cancel-below.sh 2>/dev/null
  echo "exit $?" >> "$LOG"
}
logged() { grep -qxF -- "$1" "$LOG"; }
none() { ! grep -qE '^(run cancel|run rerun|api )' "$LOG"; }

# The new top on another pull request's branch: both live runs of the layer
# below, for its current head, are cancelled and their changes job re-run.
run pull_request true feat/lower
logged "run cancel 101 -R o/r" && ok || bad "the in-progress run is cancelled"
logged "run cancel 102 -R o/r" && ok || bad "the queued run is cancelled"
logged "api -X POST repos/o/r/actions/jobs/9001/rerun" && ok || bad "its changes job is re-run"
[ "$(grep -c 'actions/jobs/9001/rerun' "$LOG")" -eq 2 ] && ok || bad "one re-run per cancelled run"
grep -q 'cancel 103' "$LOG" && bad "a completed run is left alone" || ok
grep -q 'cancel 104' "$LOG" && bad "a run of an older head is left alone" || ok
grep -q 'cancel 105' "$LOG" && bad "a merge-queue run is never cancelled" || ok
grep -q 'cancel 106' "$LOG" && bad "a push run is never cancelled" || ok
grep -q 'jobs/9002' "$LOG" && bad "only the changes job is re-run" || ok
logged "exit 0" && ok || bad "it exits 0"
grep -qE -- '^pr list -R o/r --head feat/lower --state open' "$LOG" && ok || bad "the layer below is found by head == base"
grep -qE -- '^run list -R o/r --workflow ci.yml --branch feat/lower --event pull_request' "$LOG" && ok ||
  bad "only that branch's pull_request runs of ci.yml are listed"

# Nothing happens off a pull request, below the top, on the default branch,
# or with no pull request beneath.
for event in push merge_group workflow_dispatch; do
  run "$event" true feat/lower; none && ok || bad "event $event cancels nothing"
done
run pull_request false feat/lower; none && ok || bad "a lower layer cancels nothing"
run pull_request unknown feat/lower; none && ok || bad "top=unknown cancels nothing"
run pull_request true main; none && ok || bad "a pull request on main has no layer below"
STUB_PR_OVERRIDE='[]' run pull_request true feat/lower; none && ok || bad "no pull request beneath: nothing"

# Failures are reported and never fail the run.
run pull_request true feat/lower cancel
grep -q 'rerun' "$LOG" && bad "a run that could not be cancelled is not re-run" || ok
logged "exit 0" && ok || bad "a refused cancel still exits 0"
run pull_request true feat/lower rerun
logged "exit 0" && ok || bad "a refused re-run still exits 0"

# A run cancelled before its first job has no changes job: re-run it whole.
: > "$LOG"
LOG="$LOG" PATH="$stub:$PATH" EVENT=pull_request TOP=true BASE_REF=feat/lower DEFAULT_BRANCH=main \
  REPO=o/r WAIT_SECONDS=5 STUB_PR="$PR" STUB_RUNS="$RUNS" STUB_JOBS='{"jobs":[]}' \
  bash scripts/ci-cancel-below.sh 2>/dev/null
logged "run rerun 101 -R o/r" && ok || bad "a run with no jobs yet is re-run whole"

EVENT=push bash scripts/ci-cancel-below.sh extra >/dev/null 2>&1 && bad "an argument is refused" || ok

# The workflow: its own job, after changes, allowed to write actions,
# needed by ci-ok; unit needs changes, so re-running changes re-runs it.
wf=.github/workflows/ci.yml
block() { awk -v j="  $1:" '$0 == j {f=1; next} f && /^  [a-z]/ {f=0} f' "$wf"; }
block below | grep -q 'bash scripts/ci-cancel-below.sh' && ok || bad "the below job runs ci-cancel-below.sh"
block below | grep -q 'actions: write' && ok || bad "the below job may cancel and re-run"
block below | grep -q '^    needs: \[changes' && ok || bad "the below job needs changes for its top answer"
block ci-ok | grep -q 'needs: \[changes-gate, changes, below-gate, below,' && ok || bad "ci-ok needs the below job"
block unit | grep -q '^    needs: \[changes' && ok || bad "unit needs changes, so the re-run brings it back"
for j in cli semver; do
  block "$j" | grep -q '^    needs: \[changes' && ok || bad "$j needs changes, so the re-run brings it back"
done

[ "$fail" -eq 0 ] || { printf 'ci-cancel-below: %d passed, %d failed\n' "$pass" "$fail"; exit 1; }
printf 'ci-cancel-below: all %d checks passed\n' "$pass"
