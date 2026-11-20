#!/usr/bin/env bash
# A new top of a stack stops the full run of the layer below it.
#
#   ci-cancel-below.sh   run by the changes job of a pull request after
#                        scripts/ci-heavy.sh, with its answer in TOP
#
# Owner-approved trial, 2026-11-20, part two. Pull requests opened one at a
# time are each the top when opened, so each starts the full suite. When a
# pull request is the top of its stack and is based on another pull
# request's branch (not on the default branch), the layer below has stopped
# being the top, and the full run it started is no longer wanted. So:
#
#   1. find the open pull request whose head branch is this one's base;
#   2. cancel its CI runs that are queued or in progress for its current
#      head commit -- pull_request runs only, never a merge-queue run or a
#      push to main, which run on other branches and events;
#   3. wait for each to finish cancelling, then re-run that run's changes
#      job. A re-run of a job re-runs every job that needs it, and the
#      changes job asks scripts/ci-heavy.sh again: the layer below is no
#      longer the top, so the heavy jobs are skipped by design and only the
#      fast jobs run again. ci-ok in the re-run judges that as green
#      (scripts/ci-verdict.sh); every fast job needs changes, so none of
#      them is left cancelled.
#
# Anything it cannot do -- a fork's read-only token, a run already gone --
# is reported and skipped: this saves runner time, and must never fail the
# pull request that runs it.
#
# Inputs, from the workflow's environment:
#   EVENT            github.event_name
#   TOP              ci-heavy.sh's `top` answer: true, false or unknown
#   BASE_REF         the pull request's base branch (github.base_ref)
#   DEFAULT_BRANCH   the repository's default branch
#   REPO             owner/name
#   WORKFLOW         the workflow file to act on (ci.yml)
#   WAIT_SECONDS     how long to wait for a cancelled run to finish (300)
#   GH_TOKEN         a token that may read pull requests and write actions
set -uo pipefail

[ $# -eq 0 ] || { echo "usage: ci-cancel-below.sh   (configured by its environment)" >&2; exit 2; }
note() { echo "ci-cancel-below: $*" >&2; }

[ "${EVENT:-}" = pull_request ] || { note "event ${EVENT:-none}: nothing to do"; exit 0; }
[ "${TOP:-}" = true ] || { note "not the top of its stack (top=${TOP:-}): nothing to do"; exit 0; }
[ -n "${BASE_REF:-}" ] && [ -n "${REPO:-}" ] && [ -n "${DEFAULT_BRANCH:-}" ] ||
  { note "BASE_REF, REPO or DEFAULT_BRANCH is empty: nothing done"; exit 0; }
[ "$BASE_REF" != "$DEFAULT_BRANCH" ] || { note "based on $DEFAULT_BRANCH: no layer below"; exit 0; }
workflow="${WORKFLOW:-ci.yml}"
wait="${WAIT_SECONDS:-300}"

below="$(gh pr list -R "$REPO" --head "$BASE_REF" --state open \
  --json number,headRefOid --jq '.[0] | select(.) | "\(.number) \(.headRefOid)"' 2>/dev/null)" ||
  { note "the pull request on $BASE_REF cannot be listed: nothing done"; exit 0; }
[ -n "$below" ] || { note "no open pull request has head $BASE_REF: nothing to do"; exit 0; }
number="${below%% *}"; sha="${below#* }"

runs="$(gh run list -R "$REPO" --workflow "$workflow" --branch "$BASE_REF" --event pull_request \
  --limit 20 --json databaseId,status,headSha,event \
  --jq ".[] | select(.event == \"pull_request\" and .headSha == \"$sha\" and
         (.status == \"queued\" or .status == \"in_progress\" or .status == \"waiting\"
          or .status == \"pending\" or .status == \"requested\")) | .databaseId" 2>/dev/null)" ||
  { note "the runs of #$number cannot be listed: nothing done"; exit 0; }
[ -n "$runs" ] || { note "#$number has no run in progress: nothing to do"; exit 0; }

for run in $runs; do
  if ! gh run cancel "$run" -R "$REPO" >/dev/null 2>&1; then
    note "run $run of #$number cannot be cancelled (a read-only token?): left running"
    continue
  fi
  note "cancelled run $run of #$number (head $sha): the layer below is no longer the top"
  waited=0
  while :; do
    status="$(gh run view "$run" -R "$REPO" --json status --jq .status 2>/dev/null || echo unknown)"
    [ "$status" = completed ] && break
    [ "$waited" -ge "$wait" ] && { note "run $run is still $status after ${wait}s: not re-run"; continue 2; }
    sleep 5; waited=$((waited + 5))
  done
  job="$(gh run view "$run" -R "$REPO" --json jobs \
    --jq '.jobs[] | select(.name | startswith("changes")) | .databaseId' 2>/dev/null | head -n 1)"
  if [ -z "$job" ]; then
    # The run was cancelled before its first job started: re-run it whole,
    # and its changes job decides afresh.
    gh run rerun "$run" -R "$REPO" >/dev/null 2>&1 &&
      note "re-ran run $run whole: it had no changes job yet" ||
      note "run $run cannot be re-run"
    continue
  fi
  if gh api -X POST "repos/$REPO/actions/jobs/$job/rerun" >/dev/null 2>&1; then
    note "re-ran the changes job ($job) of run $run and the jobs that need it: the fast checks"
  else
    note "the changes job ($job) of run $run cannot be re-run"
  fi
done
exit 0
