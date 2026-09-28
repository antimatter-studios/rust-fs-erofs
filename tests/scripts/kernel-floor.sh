#!/usr/bin/env bash
# The kernel tier's floor catches the loss of any one kernel test.
#
# The tier is small -- every test in it boots nothing new but mounts one of
# our images with the real kernel in the guest -- so a floor below the number
# of tests it holds lets one of them vanish unnoticed. That happened: #139
# added the readlink kernel oracle, CI started printing `kernel: 3 tests
# executed (floor 2)`, and a run that lost that test would still have passed.
#
# So this counts the `#[test]` functions in the files scripts/test-targets.sh
# selects for the tier, feeds scripts/test-floor.sh a kernel log that executed
# ONE FEWER than that, against the floor chores.yml declares, and requires it
# to refuse. A floor at or above the count passes; a test added without
# raising the floor fails here, naming both numbers.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

printf 'kernel-floor\n'

floors="$(grep -oE 'scripts/test-floor\.sh kernel [0-9]+' chores.yml | awk '{ print $3 }')"
if [ "$(printf '%s\n' "$floors" | grep -c .)" -ne 1 ]; then
    bad "chores.yml must declare exactly one kernel floor (found: ${floors:-none})"
    floors=0
fi
floor="$floors"

declared=0
files=0
for target in $(bash scripts/test-targets.sh kernel); do
    [ "$target" = --test ] && continue
    files=$((files + 1))
    declared=$((declared + $(grep -c '#\[test\]' "tests/$target.rs")))
done
if [ "$files" -gt 0 ] && [ "$declared" -gt 0 ]; then ok; else
    bad "scripts/test-targets.sh kernel selected $files files holding $declared tests"
fi

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/scripts" "$sandbox/tmp/logs"
cp scripts/test-floor.sh "$sandbox/scripts/"
short=$((declared - 1))
printf 'test result: ok. %d passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' \
    "$short" > "$sandbox/tmp/logs/kernel.log"
if bash "$sandbox/scripts/test-floor.sh" kernel "$floor" > /dev/null 2>&1; then
    bad "the kernel tier holds $declared tests but its floor is $floor: a run that executed $short still passes"
else
    ok
fi

printf 'test result: ok. %d passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' \
    "$declared" > "$sandbox/tmp/logs/kernel.log"
if bash "$sandbox/scripts/test-floor.sh" kernel "$floor" > /dev/null 2>&1; then ok; else
    bad "a kernel run that executed all $declared tests is refused by floor $floor"
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
