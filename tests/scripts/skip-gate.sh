#!/usr/bin/env bash
# A tier that ignored a test fails, however many others it ran (#129).
#
# scripts/test-floor.sh is the check every cargo tier ends with. It counted
# the tests that PASSED and compared them with the floor, and read nothing
# else off the result lines -- so a test marked `#[ignore]`, or one a
# selection opted out, left `N passed; 0 failed; 1 ignored` behind and the
# tier stayed green as long as N cleared the floor. A test that decided not
# to run is not a test that passed.
#
# So this feeds test-floor.sh logs that clear the floor and carry an ignored
# test, and requires it to refuse them, naming the test; and a log with none
# ignored, which it must still accept.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

printf 'skip-gate\n'

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/scripts" "$sandbox/tmp/logs"
cp scripts/test-floor.sh "$sandbox/scripts/"
log="$sandbox/tmp/logs/demo.log"

# One ignored test in the second of two binaries, well above the floor.
cat > "$log" <<'LOG'
running 3 tests
test a ... ok
test b ... ok
test c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 2 tests
test needs_a_fixture ... ignored, needs tests/fixtures/system.img
test d ... ok

test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
LOG
out="$(bash "$sandbox/scripts/test-floor.sh" demo 2 2>&1)"; status=$?
if [ "$status" -ne 0 ]; then ok; else
    bad "a tier that ignored a test passed its floor check: $out"
fi
case "$out" in
    *needs_a_fixture*) ok ;;
    *) bad "the refusal does not name the ignored test: $out" ;;
esac

# The same log with nothing ignored is accepted.
sed -e '/\.\.\. ignored/d' -e 's/1 ignored/0 ignored/' "$log" > "$log.clean"
mv "$log.clean" "$log"
if bash "$sandbox/scripts/test-floor.sh" demo 2 > /dev/null 2>&1; then ok; else
    bad "a tier with nothing ignored was refused"
fi

# An ignored count with no per-test line (a harness that prints only the
# summary) is still refused: the number is the verdict, the name a courtesy.
printf 'test result: ok. 5 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out\n' > "$log"
if bash "$sandbox/scripts/test-floor.sh" demo 2 > /dev/null 2>&1; then
    bad "a summary line reporting 2 ignored passed the floor check"
else
    ok
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
