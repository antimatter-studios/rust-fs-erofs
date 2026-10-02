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
# selects for the tier and requires the floor chores.yml declares to EQUAL
# that count: a floor below it lets a run one test short pass, and a floor
# above it refuses a run that executed every test. The floor itself is
# rust-fs-core's (scripts/core.sh test-floor), tested there; what is this
# repository's is the number, so the number is what is checked here.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

printf 'kernel-floor\n'

floors="$(grep -oE 'core\.sh test-floor (--refuse-ignored )?kernel [0-9]+' chores.yml | awk '{ print $NF }')"
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

short=$((declared - 1))
if [ "$floor" -gt "$short" ]; then ok; else
    bad "the kernel tier holds $declared tests but its floor is $floor: a run that executed $short still passes"
fi
if [ "$floor" -le "$declared" ]; then ok; else
    bad "a kernel run that executed all $declared tests is refused by floor $floor"
fi
printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
