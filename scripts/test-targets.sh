#!/usr/bin/env bash
#
# test-targets.sh unit|images|oracle|kernel|gsi|all — print the `cargo test`
# target arguments for one tier of the suite.
#
#   unit    the library, the binaries, and every tests/*.rs that reaches
#           neither a fixture nor the VM: no tool, no kernel, no harness
#   images  the tests that read a fixture but need no VM — everything a
#           host without KVM (GitHub's arm64 runners) can still run
#   oracle  every tests/*.rs that runs an erofs-utils tool (in the VM)
#   kernel  every tests/*.rs that mounts one of our images with the real
#           kernel (in the VM)
#   all     every target the four tiers above cover, in one selection:
#           what `chore test:native` runs once as the whole suite. NOT
#           `cargo test` with no arguments, which would also pick up
#           `gsi` and fail on a fixture that machine cannot have
#   gsi     the Android system image suite, which needs a ~2 GiB fixture
#           that is not ours to redistribute. ITS OWN TIER AND IN NO
#           OTHER: `chore test` does not run it and CI cannot. Inside it
#           a missing fixture fails (#54); what it must never be again is
#           a test that reports ok having opened nothing.
#
# DERIVED FROM THE TESTS THEMSELVES, not from a list someone has to keep:
# a test names a fixture by its test-disks/ path or through
# `fs_erofs_test_support::fixture(env!(...))`, reaches a tool only
# through `oracle` / `assert_fsck_clean` / `mkfs_from_guest_tree` (and
# the `common` wrappers over them), and the kernel only through
# `guest_kernel_*` — the helpers that fail, never skip, when the VM or
# the tool is missing. tests/test_contract.rs fails if a test reaches
# either any other way, so the classification cannot drift.
#
# THE TIERS ARE DISJOINT, so the tasks together run each file once. A
# file that reaches more than one oracle is placed at the most specific:
# the kernel first, then the tools. A kernel test that also builds its
# image with `mkfs.erofs` has the kernel's verdict.
#
# Library tests that need a fixture or the VM would live in modules named
# `needs_host`; `unit` excludes them with cargo's name filter. There are
# none today — src/ is pure parsing and writing — and the filter is what
# keeps that true by construction rather than by inspection.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
HOST='test-disks|fixture\(env!|oracle\(|assert_fsck_clean\(|mkfs_from_guest_tree\(|guest_kernel_|_mkfs_erofs\('
VM='oracle\(|assert_fsck_clean\(|mkfs_from_guest_tree\(|guest_kernel_|_mkfs_erofs\('
TOOL='oracle\(|assert_fsck_clean\(|mkfs_from_guest_tree\(|_mkfs_erofs\('
KERNEL='guest_kernel_'
# Named, not derived: it is the one suite defined by a fixture nobody can
# ship rather than by what it calls.
GSI=oracle_gsi

tier="${1:-}"
args=()
for f in "$REPO"/tests/*.rs; do
    name="$(basename "$f" .rs)"
    if [ "$name" = "$GSI" ]; then
        [ "$tier" = gsi ] && args+=(--test "$name")
        continue
    fi
    case "$tier" in
        all) args+=(--test "$name") ;;
        unit) grep -qE "$HOST" "$f" || args+=(--test "$name") ;;
        images)
            if grep -qE "$HOST" "$f" && ! grep -qE "$VM" "$f"; then
                args+=(--test "$name")
            fi
            ;;
        oracle)
            if grep -qE "$TOOL" "$f" && ! grep -qE "$KERNEL" "$f"; then
                args+=(--test "$name")
            fi
            ;;
        kernel) grep -qE "$KERNEL" "$f" && args+=(--test "$name") ;;
        gsi) ;;
        *) echo "usage: test-targets.sh unit|images|oracle|kernel|gsi|all" >&2; exit 2 ;;
    esac
done
case "$tier" in
    unit) printf '%s\n' --lib --bins "${args[@]}" -- --skip needs_host:: ;;
    all) printf '%s\n' --lib --bins "${args[@]}" ;;
    *) printf '%s\n' "${args[@]}" ;;
esac
