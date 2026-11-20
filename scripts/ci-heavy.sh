#!/usr/bin/env bash
# Whether this run takes the full suite or the fast checks only.
#
#   ci-heavy.sh   prints `heavy=true|false` and `top=true|false|unknown`,
#                 and a reason on stderr; the changes job and ci-ok write
#                 them to their outputs
#
# Owner-approved trial, 2026-11-20: a stack of pull requests runs the full
# suite on its TOP layer only. Every layer runs the fast tier (fmt, clippy,
# unit, the CLI, semver, the shell guards); the heavy jobs -- the fixtures
# built in the harness VM and everything that reads them (the oracle and
# kernel tiers on x86_64, the aarch64 tiers, the suite in the guest), the
# LZ4HC benchmark and the Windows CLI legs -- run when this says true:
#
#   * the merge queue, a push to main or a tag, a manual run: always. The
#     queue tests main plus every queued layer with everything, so nothing
#     lands less tested than before the trial;
#   * a pull request that is the top of its stack: no OPEN pull request
#     is based on its head branch;
#   * a pull request carrying the `full-ci` label, read live, so labelling
#     a layer and re-running its CI (`gh run rerun <id>`) gives it the full
#     suite. That is how to bisect a stack: label the middle layer, re-run,
#     and halve towards the layer that broke.
#
# A layer pushed on top becomes the top and runs the full suite on its own
# push. A full run the layer below started while it was the top is
# cancelled by the new top, and only its fast checks run again
# (scripts/ci-cancel-below.sh); results it already has are kept. If the pull requests cannot be listed, the answer is true: a run
# that cannot tell is a full run, never a fast one.
#
# Inputs, from the workflow's environment:
#   EVENT        github.event_name
#   HEAD_REF     the pull request's head branch (github.head_ref)
#   PR_NUMBER    the pull request's number
#   REPO         owner/name
#   GH_TOKEN     a token that may read pull requests (the gh CLI reads it)
set -uo pipefail

[ $# -eq 0 ] || { echo "usage: ci-heavy.sh   (configured by its environment)" >&2; exit 2; }

# Prints `heavy=` and `top=` (whether no open pull request is stacked on
# this one; `top=unknown` outside a pull request or when it cannot be
# told), and the reason on stderr.
say() { # heavy top reason
  echo "heavy=$1"
  echo "top=$2"
  echo "ci-heavy: heavy=$1 top=$2 ($3)" >&2
  exit 0
}

case "${EVENT:-}" in
  pull_request) ;;
  "") echo "ci-heavy: EVENT is empty" >&2; exit 2 ;;
  *) say true unknown "event ${EVENT}: every leg runs" ;;
esac

[ -n "${HEAD_REF:-}" ] && [ -n "${PR_NUMBER:-}" ] && [ -n "${REPO:-}" ] ||
  say true unknown "a pull request without HEAD_REF, PR_NUMBER or REPO"

above="$(gh pr list -R "$REPO" --base "$HEAD_REF" --state open --json number --jq 'length' 2>/dev/null)" ||
  say true unknown "the pull requests based on $HEAD_REF cannot be listed"
case "$above" in
  0) top=true ;;
  ''|*[!0-9]*) say true unknown "the count of pull requests on $HEAD_REF is '$above'" ;;
  *) top=false ;;
esac

labels="$(gh pr view "$PR_NUMBER" -R "$REPO" --json labels --jq '.labels[].name' 2>/dev/null)" ||
  say true "$top" "the labels of #$PR_NUMBER cannot be read"
if printf '%s\n' "$labels" | grep -qx 'full-ci'; then
  say true "$top" "#$PR_NUMBER carries full-ci"
fi
if [ "$top" = true ]; then
  say true true "#$PR_NUMBER is the top of its stack"
fi
say false false "#$PR_NUMBER has $above open pull request(s) stacked on $HEAD_REF; the top runs the full suite"
