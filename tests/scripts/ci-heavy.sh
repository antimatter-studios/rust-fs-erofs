#!/usr/bin/env bash
# Truth table for scripts/ci-heavy.sh: which runs take the full suite.
#
# `gh` is replaced by a stub on PATH that answers from the environment, so
# every case is decided offline: STUB_LABELS (the pull request's labels,
# one per line), STUB_ABOVE (how many open pull requests are based on its
# head branch), STUB_FAIL (view or list, to make that call fail).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

stub="$(mktemp -d "${TMPDIR:-/tmp}/ci-heavy.XXXXXX")"
trap 'rm -rf "$stub"' EXIT
cat > "$stub/gh" <<'GH'
#!/usr/bin/env bash
case "$1 $2" in
  "pr view")
    [ "${STUB_FAIL:-}" = view ] && exit 1
    printf '%s\n' "${STUB_LABELS:-}" ;;
  "pr list")
    [ "${STUB_FAIL:-}" = list ] && exit 1
    # The script must ask for open pull requests based on its head branch.
    case " $* " in *" --base feat/layer "*"--state open "*) ;; *) echo "bad query: $*" >&2; exit 3 ;; esac
    printf '%s\n' "${STUB_ABOVE:-0}" ;;
  *) exit 4 ;;
esac
GH
chmod +x "$stub/gh"

heavy() { # want event [labels] [above] [fail]
  local want="$1" event="$2" got
  got="$(PATH="$stub:$PATH" EVENT="$event" HEAD_REF=feat/layer PR_NUMBER=7 REPO=o/r \
    STUB_LABELS="${3:-}" STUB_ABOVE="${4:-0}" STUB_FAIL="${5:-}" bash scripts/ci-heavy.sh 2>/dev/null)"
  [ "$got" = "heavy=$want" ] && ok ||
    bad "event=$event labels=[${3:-}] above=${4:-0} fail=${5:-}: wanted heavy=$want, got '$got'"
}

heavy true  merge_group                      # the queue runs everything
heavy true  push
heavy true  workflow_dispatch
heavy true  pull_request "" 0                # the top of its stack
heavy false pull_request "" 1                # a lower layer: fast checks
heavy false pull_request "" 3
heavy false pull_request "other-label" 1     # only full-ci asks for the full suite
heavy true  pull_request "full-ci" 1         # a labelled lower layer runs everything
heavy true  pull_request "x
full-ci" 2
heavy false pull_request "full-ci-not" 1     # the whole label, not a prefix
heavy true  pull_request "" 1 view           # cannot read labels: full run
heavy true  pull_request "" 1 list           # cannot list the stack: full run
heavy true  pull_request "" "many"           # an answer that is not a count: full run

out="$(PATH="$stub:$PATH" EVENT=pull_request HEAD_REF= PR_NUMBER= REPO= bash scripts/ci-heavy.sh 2>/dev/null)"
[ "$out" = heavy=true ] && ok || bad "a pull request without its head branch runs everything, got '$out'"
EVENT= bash scripts/ci-heavy.sh >/dev/null 2>&1 && bad "an empty EVENT is refused" || ok
EVENT=push bash scripts/ci-heavy.sh extra >/dev/null 2>&1 && bad "an argument is refused" || ok

# The workflow wires it: the changes job writes heavy, every heavy job is
# gated on it or needs a job that is, and ci-ok hands HEAVY to the verdict.
wf=.github/workflows/ci.yml
grep -q 'heavy="$(bash scripts/ci-heavy.sh)"' "$wf" && ok || bad "the changes job runs ci-heavy.sh"
grep -q 'heavy: ${{ steps.h.outputs.heavy }}' "$wf" && ok || bad "the changes job outputs heavy"
grep -q 'HEAVY: ${{ needs.changes.outputs.heavy }}' "$wf" && ok || bad "ci-ok passes HEAVY to the verdict"
grep -q 'pull-requests: read' "$wf" && ok || bad "the changes job may read pull requests"
for job in benchmark fixtures cli-windows; do
  awk -v j="  $job:" '$0 == j {f=1; next} f && /^  [a-z]/ {f=0} f' "$wf" |
    grep -q "needs.changes.outputs.heavy == 'true'" && ok || bad "$job is gated on heavy"
done
for job in test test-arm64 suite-in-vm; do
  awk -v j="  $job:" '$0 == j {f=1; next} f && /^  [a-z]/ {f=0} f' "$wf" |
    grep -q '^    needs: fixtures' && ok || bad "$job needs fixtures, so it is skipped with it"
done

[ "$fail" -eq 0 ] || { printf 'ci-heavy: %d passed, %d failed\n' "$pass" "$fail"; exit 1; }
printf 'ci-heavy: all %d checks passed\n' "$pass"
