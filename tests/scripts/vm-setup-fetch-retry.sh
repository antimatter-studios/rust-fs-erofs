#!/usr/bin/env bash
# The erofs-utils fetch in scripts/vm-setup.sh survives a server that answers
# with an error once, and still fails, naming what it tried, when the server
# never answers (#289).
#
# git.kernel.org answered one fetch with HTTP 502, and since the script
# fetched exactly once, that one answer failed the fixtures job and took a
# pull request out of the merge queue: #260 in run 42891268715 and #290 in
# run 42916129946, both on the same day.
#
# The harness ships vm-setup.sh into the guest as one file, so the retry
# lives in it, as `fetch_pinned`. This guard takes that function out of the
# script and runs it against a stub `git` first on PATH. The stub fails the
# number of fetches named in its counter file, the way the real one does,
# and then succeeds. Nothing here touches the network or needs root.
#
#   bash tests/scripts/vm-setup-fetch-retry.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SETUP="$REPO/scripts/vm-setup.sh"

fail() { echo "FAIL  $*" >&2; exit 1; }

command -v timeout >/dev/null 2>&1 ||
    fail "timeout(1) is required (coreutils; on macOS: brew install coreutils)"

fn="$(sed -n '/^fetch_pinned() {$/,/^}$/p' "$SETUP")"
[ -n "$fn" ] ||
    fail "scripts/vm-setup.sh defines no fetch_pinned(): its erofs-utils fetch is tried once, and one HTTP 502 fails the fixtures job (#289)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/src"
URL=https://git.example.invalid/erofs-utils.git

# The stub: `remote get-url origin` answers the URL; `fetch` fails while the
# counter is above zero, counting down, with the message the real one printed.
cat > "$work/bin/git" <<EOF
#!/usr/bin/env bash
args=" \$* "
case "\$args" in
    *" remote get-url origin "*) echo "$URL"; exit 0 ;;
    *" fetch "*)
        echo fetch >> "$work/calls"
        left=\$(cat "$work/fail")
        if [ "\$left" -gt 0 ]; then
            echo \$((left - 1)) > "$work/fail"
            echo "error: RPC failed; HTTP 502 curl 22 The requested URL returned error: 502" >&2
            echo "fatal: expected flush after ref listing" >&2
            exit 128
        fi
        exit 0 ;;
esac
echo "stub git: unexpected arguments: \$*" >&2
exit 2
EOF
chmod +x "$work/bin/git"

# One run of fetch_pinned in a shell of its own, under `set -e` like the
# script, with the stub first on PATH and no pause between attempts.
run() {
    echo "$1" > "$work/fail"
    : > "$work/calls"
    PATH="$work/bin:$PATH" VM_SETUP_FETCH_DELAY=0 VM_SETUP_FETCH_ATTEMPTS=4 \
        bash -c "set -euo pipefail; $fn"$'\n'"fetch_pinned \"$work/src\" v1.9.1" \
        > "$work/out" 2>&1
}
calls() { wc -l < "$work/calls" | tr -d ' '; }

run 0 || fail "a fetch that succeeds first time failed: $(cat "$work/out")"
[ "$(calls)" = 1 ] || fail "a fetch that succeeds first time was made $(calls) times"

run 1 || fail "one HTTP 502 still failed the fetch: $(cat "$work/out")"
[ "$(calls)" = 2 ] || fail "after one HTTP 502 the fetch was made $(calls) times, not 2"
grep -q "attempt 1 of 4" "$work/out" ||
    fail "a failed attempt does not say which attempt it was: $(cat "$work/out")"

run 3 || fail "three HTTP 502s failed a fetch allowed four attempts: $(cat "$work/out")"
[ "$(calls)" = 4 ] || fail "after three HTTP 502s the fetch was made $(calls) times, not 4"

if run 99; then fail "a fetch that never succeeds was reported as fetched"; fi
[ "$(calls)" = 4 ] || fail "a fetch that never succeeds was made $(calls) times, not the 4 allowed"
grep -q "$URL" "$work/out" ||
    fail "the final failure does not name the URL it fetched from: $(cat "$work/out")"
grep -q "after 4 attempts" "$work/out" ||
    fail "the final failure does not say how many attempts it made: $(cat "$work/out")"

echo "ok    vm-setup's erofs-utils fetch retries a failed attempt and names what it tried"
