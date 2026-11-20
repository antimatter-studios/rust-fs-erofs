#!/usr/bin/env bash
# Whether ci-ok may call a run green.
#
#   ci-verdict.sh   reads the jobs' toJSON(needs) on stdin, with CODE, HEAVY
#                   and HEAVY_JOBS in the environment, and exits 1 naming
#                   every job that is not green
#
# Every event runs every leg: a pull request, the merge queue, a push to main
# or a tag, and a manual run alike. A pull request used to run a quick
# profile of unit, lint and semver (#205), which left the merge queue as the
# first run of the oracle, kernel and CLI legs for any change. A break there
# then surfaced in a queue group instead of on the pull request that caused
# it, and could remove other pull requests from the queue with it. Speed is
# not worth that.
#
# A skipped job is accepted only when the change is documentation alone
# (CODE=false), which skips the jobs that boot the VM or read its fixtures,
# or -- the owner-approved trial of 2026-11-20 -- when the job is one of the
# HEAVY_JOBS and this run is a lower layer of a stack (HEAVY=false,
# scripts/ci-heavy.sh). The top layer, a `full-ci` layer and the merge queue
# run everything. A failed job is never accepted, and a skip of a job
# outside HEAVY_JOBS is a failure whatever HEAVY says.
#
# HEAVY_NOW is the same question asked again when ci-ok runs. A layer that
# was the top when its run started and is not any more has had that run's
# heavy jobs cancelled by the new top (scripts/ci-cancel-below.sh). With
# HEAVY_NOW=false a HEAVY_JOBS job that was cancelled is accepted like one
# that was skipped; any other cancelled job is still a failure. HEAVY_NOW
# defaults to HEAVY.
set -euo pipefail

[ $# -eq 0 ] || { echo "usage: ci-verdict.sh < needs.json" >&2; exit 2; }
code="${CODE:-}"
case "$code" in
  true|false) ;;
  *) echo "::error::ci-verdict.sh: CODE is '$code', not true or false"; exit 1 ;;
esac
heavy="${HEAVY:-}"
case "$heavy" in
  true|false) ;;
  *) echo "::error::ci-verdict.sh: HEAVY is '$heavy', not true or false"; exit 1 ;;
esac
heavy_now="${HEAVY_NOW:-$heavy}"
case "$heavy_now" in
  true|false) ;;
  *) echo "::error::ci-verdict.sh: HEAVY_NOW is '$heavy_now', not true or false"; exit 1 ;;
esac
heavy_jobs="${HEAVY_JOBS:-}"
{ [ "$heavy" = true ] && [ "$heavy_now" = true ]; } || [ -n "$heavy_jobs" ] ||
  { echo "::error::ci-verdict.sh: HEAVY is false and HEAVY_JOBS names no job"; exit 1; }
command -v jq >/dev/null || { echo "::error::ci-verdict.sh: jq is required"; exit 1; }
needs="$(cat)"
echo "$needs" | jq -r 'to_entries[] | "\(.key): \(.value.result)"'
echo "code changed: $code"
echo "full suite: $heavy at the start, $heavy_now now${heavy_jobs:+ (heavy jobs: $heavy_jobs)}"
bad="$(echo "$needs" | jq -r --arg code "$code" --arg heavy "$heavy" --arg now "$heavy_now" \
  --arg jobs "$heavy_jobs" '
  ($jobs | split(" ") | map(select(. != ""))) as $heavyjobs
  | [to_entries[]
     | select(.value.result != "success")
     | (.key | IN($heavyjobs[])) as $isheavy
     | select((
         (.value.result == "skipped"
          and ($code == "false" or (($heavy == "false" or $now == "false") and $isheavy)))
         or (.value.result == "cancelled" and $now == "false" and $isheavy)
       ) | not)
     | .key] | join(", ")')"
if [ -n "$bad" ]; then
  echo "::error::not green: $bad (failed, cancelled or skipped)"
  exit 1
fi
echo "all required jobs succeeded"
