#!/usr/bin/env bash
# Which CI profile an event runs, and whether ci-ok may call a run green.
#
#   ci-profile.sh profile <event>   prints quick or full
#   ci-profile.sh verdict           reads the jobs' toJSON(needs) on stdin,
#                                   with CODE and PROFILE in the environment,
#                                   and exits 1 naming every job that is not
#                                   green
#
# A pull request runs the quick profile: the unit tier, lint, the shell tests
# and semver. The merge queue, a push to main or a tag, and a manual
# workflow_dispatch run the full profile: every leg, including the fixture
# VM, the oracle and kernel tiers, x86_64 and arm64, the suite in the guest,
# and the CLI on Linux and both Windows architectures. Any other event runs
# the full profile too: an event nobody classified should run more, not less.
#
# ci-ok accepts a skipped job only when the change is documentation alone
# (CODE=false), or when the run is quick and the job is one of the full-only
# legs below. A failed or cancelled job is never accepted, and neither is a
# skipped quick-tier job on any profile.
set -euo pipefail

FULL_ONLY='["fixtures","test","test-arm64","suite-in-vm","cli","cli-windows"]'

case "${1:-}" in
  profile)
    case "${2:-}" in
      pull_request) echo quick ;;
      "") echo "ci-profile.sh: profile needs an event name" >&2; exit 2 ;;
      *) echo full ;;
    esac
    ;;
  verdict)
    code="${CODE:-}"
    profile="${PROFILE:-}"
    case "$profile" in
      quick|full) ;;
      *) echo "::error::ci-profile.sh: PROFILE is '$profile', not quick or full"; exit 1 ;;
    esac
    command -v jq >/dev/null || { echo "::error::ci-profile.sh: jq is required"; exit 1; }
    needs="$(cat)"
    echo "$needs" | jq -r 'to_entries[] | "\(.key): \(.value.result)"'
    echo "code changed: $code, profile: $profile"
    bad="$(echo "$needs" | jq -r --arg code "$code" --arg profile "$profile" --argjson full "$FULL_ONLY" '
      [to_entries[]
       | select(.value.result != "success")
       | select((.value.result == "skipped"
                 and ($code == "false"
                      or ($profile == "quick" and (.key as $k | $full | index($k)) != null))) | not)
       | .key] | join(", ")')"
    if [ -n "$bad" ]; then
      echo "::error::not green: $bad (failed, cancelled or skipped)"
      exit 1
    fi
    echo "all required jobs succeeded"
    ;;
  *)
    echo "usage: ci-profile.sh profile <event> | verdict" >&2
    exit 2
    ;;
esac
