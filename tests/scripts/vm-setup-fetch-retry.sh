#!/usr/bin/env bash
# The erofs-utils fetch in scripts/vm-setup.sh survives a server that answers
# with an error, moves on to the second source when the first one never
# answers, accepts only the pinned commit, and fails naming every source it
# tried when none of them answers (#289, #292).
#
# git.kernel.org answered one fetch with HTTP 502. Since the script fetched
# exactly once, that one answer failed the fixtures job and took a pull
# request out of the merge queue: #260 in run 42891268715 and #290 in run
# 42916129946. With retries in place, an outage then outlasted all four
# attempts at that host, in run 42924482422.
#
# The harness ships vm-setup.sh into the guest as one file, so all of this
# lives in it, as `fetch_pinned`. This guard takes that function out of the
# script and runs it against a stub `git` first on PATH. The stub serves two
# sources, A and B. Each source fails the number of fetches named in its
# counter file, the way the real one does, and then answers with the commit
# named in its commit file. Nothing here touches the network or needs root.
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
URL_A=https://git.example.invalid/erofs-utils.git
URL_B=https://mirror.example.invalid/erofs-utils.git
PIN=d3fbccba6e409c100843563b416c47cd82c87ca0
OTHER=0000000000000000000000000000000000000001

# The stub. `remote set-url origin` and `remote get-url origin` keep the
# current source; `fetch` fails while that source's counter is above zero,
# counting down, with the message the real one printed; `rev-parse` answers
# that source's commit.
cat > "$work/bin/git" <<EOF
#!/usr/bin/env bash
key() { [ "\$(cat "$work/url")" = "$URL_B" ] && echo b || echo a; }
args=" \$* "
case "\$args" in
    *" remote set-url origin "*) echo "\${@: -1}" > "$work/url"; exit 0 ;;
    *" remote get-url origin "*) cat "$work/url"; exit 0 ;;
    *" fetch "*)
        k=\$(key)
        echo "fetch \$k" >> "$work/calls"
        left=\$(cat "$work/fail.\$k")
        if [ "\$left" -gt 0 ]; then
            echo \$((left - 1)) > "$work/fail.\$k"
            echo "error: RPC failed; HTTP 502 curl 22 The requested URL returned error: 502" >&2
            echo "fatal: expected flush after ref listing" >&2
            exit 128
        fi
        echo "\$k" > "$work/fetched"
        exit 0 ;;
    *" rev-parse "*) cat "$work/commit.\$(cat "$work/fetched")"; exit 0 ;;
esac
echo "stub git: unexpected arguments: \$*" >&2
exit 2
EOF
chmod +x "$work/bin/git"

# One run of fetch_pinned in a shell of its own, under `set -e` like the
# script, with the stub first on PATH and no pause between attempts:
#   run <fails at A> <fails at B> [<commit A> [<commit B>]]
run() {
    echo "$1" > "$work/fail.a"
    echo "$2" > "$work/fail.b"
    echo "${3:-$PIN}" > "$work/commit.a"
    echo "${4:-$PIN}" > "$work/commit.b"
    echo "$URL_A" > "$work/url"           # as `git remote add` left it
    : > "$work/calls"
    PATH="$work/bin:$PATH" VM_SETUP_FETCH_DELAY=0 VM_SETUP_FETCH_ATTEMPTS=4 \
        bash -c "set -euo pipefail; $fn"$'\n'"fetch_pinned \"$work/src\" v1.9.1 $PIN $URL_A $URL_B" \
        > "$work/out" 2>&1
}
calls() { grep -c "fetch $1" "$work/calls"; }
out() { cat "$work/out"; }

run 0 0 || fail "a fetch that succeeds first time failed: $(out)"
[ "$(calls a)/$(calls b)" = 1/0 ] || fail "a fetch that succeeds first time was made $(calls a) times at the first source and $(calls b) at the second"

run 1 0 || fail "one HTTP 502 still failed the fetch: $(out)"
[ "$(calls a)/$(calls b)" = 2/0 ] || fail "after one HTTP 502 the fetch was made $(calls a)+$(calls b) times, not 2 at the first source"
grep -q "attempt 1 of 4" "$work/out" ||
    fail "a failed attempt does not say which attempt it was: $(out)"

run 3 0 || fail "three HTTP 502s failed a fetch allowed four attempts: $(out)"
[ "$(calls a)/$(calls b)" = 4/0 ] || fail "after three HTTP 502s the fetch was made $(calls a)+$(calls b) times, not 4 at the first source"

run 99 0 || fail "the first source failing every attempt failed the fetch, though the second source answers (#292): $(out)"
[ "$(calls a)/$(calls b)" = 4/1 ] || fail "with the first source down the fetch was made $(calls a) times there and $(calls b) at the second, not 4 and 1"

run 0 0 "$OTHER" || fail "the first source answering with the wrong commit failed the fetch, though the second source has the pin: $(out)"
[ "$(calls a)/$(calls b)" = 1/1 ] || fail "a wrong commit at the first source was not followed by one fetch at the second: $(calls a) and $(calls b)"
grep -q "$OTHER" "$work/out" ||
    fail "a fetch that answered the wrong commit does not name the commit it got: $(out)"

if run 0 0 "$OTHER" "$OTHER"; then fail "a fetch that never got the pinned commit was reported as fetched"; fi

if run 99 99; then fail "a fetch that never succeeds was reported as fetched"; fi
[ "$(calls a)/$(calls b)" = 4/4 ] || fail "a fetch that never succeeds was made $(calls a) and $(calls b) times, not the 4 allowed at each source"
if ! grep -q "$URL_A" "$work/out" || ! grep -q "$URL_B" "$work/out"; then
    fail "the final failure does not name every source it fetched from: $(out)"
fi
grep -q "after 4 attempts" "$work/out" ||
    fail "the final failure does not say how many attempts it made: $(out)"

echo "ok    vm-setup's erofs-utils fetch retries, falls back to its second source, and takes only the pinned commit"
