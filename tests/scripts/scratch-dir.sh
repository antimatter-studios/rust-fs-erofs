#!/usr/bin/env bash
# Tests for scripts/test.sh's choice of scratch directory, which differs
# depending on WHERE the suite is running and gets it wrong silently.
#
# From the host, scratch must be inside the repository: the oracle tools run in
# the harness VM, which sees this repository at the path the host knows it by
# and nothing else, so an image anywhere else is a path the tool cannot open.
#
# Inside the guest (FLTH_GUEST=1) it must be on the guest's OWN disk, because
# /repo is a virtio-9p share and mmap over 9p does not support what mkfs.erofs
# asks of it for -Efragments and -m65536. That failure is a SIGSEGV, not a
# refusal: exit 139 with nothing to report. It passes on an aarch64 host and
# fails on CI's x86_64 guest (run 36243473053), so nothing local catches a
# regression here — hence this file.
#
# Why a shell test and not a Rust one: tests/support/src/lib.rs makes the same
# choice and is covered by its own unit tests, but scripts/test.sh EXPORTS
# FS_EROFS_TEST_TMPDIR, so whatever this script picks is what the Rust default
# never gets asked about. This is the copy that wins, so this is the copy that
# needs a guard.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

# `--print-temp-dir` creates the directory, prints it and exits, which is the
# whole decision without running a suite.
got_host="$(env -u FLTH_GUEST -u FS_EROFS_TEST_TMPDIR bash scripts/test.sh --print-temp-dir)"
got_guest="$(env -u FS_EROFS_TEST_TMPDIR FLTH_GUEST=1 bash scripts/test.sh --print-temp-dir)"

printf 'scratch-dir\n'

case "$got_host" in
    "$ROOT"/tmp/*) ok ;;
    *) bad "from the host, scratch must be under $ROOT/tmp (got $got_host)" ;;
esac

case "$got_guest" in
    /var/tmp/fs-erofs-tests/*) ok ;;
    *) bad "in the guest, scratch must be on the guest's own disk, not the 9p mount (got $got_guest)" ;;
esac

# The two must not be the same place, which is the regression this exists for.
if [ "$got_host" != "$got_guest" ]; then ok; else bad "the host and the guest chose the same directory"; fi

# An explicit directory outside the repository is refused from the host and
# honoured in the guest. A refusal here is a non-zero exit, not a message.
if env -u FLTH_GUEST FS_EROFS_TEST_TMPDIR=/var/tmp/elsewhere-$$ \
        bash scripts/test.sh --print-temp-dir >/dev/null 2>&1; then
    bad "from the host, a scratch directory outside the repository must be refused"
else
    ok
fi

got_explicit="$(FLTH_GUEST=1 FS_EROFS_TEST_TMPDIR=/var/tmp/elsewhere-$$ \
    bash scripts/test.sh --print-temp-dir 2>/dev/null)"
if [ "$got_explicit" = "/var/tmp/elsewhere-$$" ]; then ok; else
    bad "in the guest, an explicit directory is taken as given (got $got_explicit)"
fi
rmdir "/var/tmp/elsewhere-$$" 2>/dev/null || true

# The Rust side has to agree, or a caller that does not go through test.sh
# lands somewhere else again.
if grep -q 'pub const GUEST_SCRATCH: &str = "/var/tmp/fs-erofs-tests"' tests/support/src/lib.rs; then
    ok
else
    bad "tests/support/src/lib.rs's GUEST_SCRATCH no longer matches this script's"
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
