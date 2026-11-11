#!/usr/bin/env bash
# Whether ci-ok may call a run green.
#
#   ci-verdict.sh   reads the jobs' toJSON(needs) on stdin, with CODE in the
#                   environment, and exits 1 naming every job that is not
#                   green
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
# (CODE=false), which skips the jobs that boot the VM or read its fixtures.
# A failed or cancelled job is never accepted.
set -euo pipefail

[ $# -eq 0 ] || { echo "usage: ci-verdict.sh < needs.json" >&2; exit 2; }
code="${CODE:-}"
case "$code" in
  true|false) ;;
  *) echo "::error::ci-verdict.sh: CODE is '$code', not true or false"; exit 1 ;;
esac
command -v jq >/dev/null || { echo "::error::ci-verdict.sh: jq is required"; exit 1; }
needs="$(cat)"
echo "$needs" | jq -r 'to_entries[] | "\(.key): \(.value.result)"'
echo "code changed: $code"
bad="$(echo "$needs" | jq -r --arg code "$code" '
  [to_entries[]
   | select(.value.result != "success")
   | select((.value.result == "skipped" and $code == "false") | not)
   | .key] | join(", ")')"
if [ -n "$bad" ]; then
  echo "::error::not green: $bad (failed, cancelled or skipped)"
  exit 1
fi
echo "all required jobs succeeded"
