#!/usr/bin/env bash
# test.sh [cargo test args...]  run the suite in an owned scratch directory
# test.sh --print-temp-dir      print that directory and exit
#
# SCRATCH LIVES IN THE REPOSITORY WHEN THE HOST DRIVES THE GUEST: tmp/
# (gitignored), and never the system temporary directory or a
# runner-supplied one. The oracle tools run inside the
# fs-linux-test-harness VM, which sees this repository at the path the
# host knows it by and nothing else of the host — so an image anywhere
# else is a path the tool asked to read it cannot open.
#
# INSIDE THE GUEST THAT INVERTS, and it has to be decided here rather
# than only in Rust: /repo is a virtio-9p share, and it is the one
# filesystem the tools must not work on. `mkfs.erofs` mmaps its output for
# -Efragments and -m65536, mmap over 9p does not support what it needs,
# and the failure is a SIGSEGV rather than a refusal — exit 139, with
# nothing to report but the number. Measured on CI run 36243473053, the
# same commands against the same build differing only in the directory:
#
#   test (x86_64, oracles in the VM)   host path, tool over ssh   -> 0
#   suite in the guest                 /repo/tmp/... (9p)         -> 139
#
# It passes on an aarch64 host, so it is not a difference a developer
# finds locally. The Rust side (tests/support/src/lib.rs,
# select_temp_dir) makes the same choice — but THIS script exports
# FS_EROFS_TEST_TMPDIR, so whatever it picks is what the Rust default
# never gets asked about. Both have to agree, and this is the one that
# wins.
#
# FS_EROFS_TEST_TMPDIR supplies an exact directory instead. From the host
# it must be inside the repository; in the guest it is taken as given,
# because there is no host for it to be invisible to. Either way it is the
# caller's to delete.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR=""

cleanup() {
    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" ]]; then
        find "$RUN_DIR" -depth -mindepth 1 -delete
        rmdir "$RUN_DIR"
    fi
}
trap cleanup EXIT HUP INT TERM

# Kept in step with GUEST_SCRATCH in tests/support/src/lib.rs. /var/tmp
# rather than /tmp: a tmpfs /tmp is sized from the guest's RAM and the
# fixture images are not all small.
GUEST_SCRATCH=/var/tmp/fs-erofs-tests

if [[ -n "${FS_EROFS_TEST_TMPDIR:-}" ]]; then
    if [[ "${FLTH_GUEST:-}" != 1 ]]; then
        case "$FS_EROFS_TEST_TMPDIR" in
            "$REPO"/*) ;;
            *)
                echo "test.sh: FS_EROFS_TEST_TMPDIR is $FS_EROFS_TEST_TMPDIR, which is outside" >&2
                echo "         $REPO. The oracle tools run in the harness VM, which sees this" >&2
                echo "         repository and nothing else of the host." >&2
                exit 1
                ;;
        esac
    fi
    # An exact caller-supplied directory is not ours to delete.
    mkdir -p "$FS_EROFS_TEST_TMPDIR"
elif [[ "${FLTH_GUEST:-}" == 1 ]]; then
    mkdir -p "$GUEST_SCRATCH"
    RUN_DIR="$(mktemp -d "$GUEST_SCRATCH/run.XXXXXX")"
    export FS_EROFS_TEST_TMPDIR="$RUN_DIR"
else
    mkdir -p "$REPO/tmp"
    RUN_DIR="$(mktemp -d "$REPO/tmp/fs-erofs-tests.XXXXXX")"
    export FS_EROFS_TEST_TMPDIR="$RUN_DIR"
fi

export TMPDIR="$FS_EROFS_TEST_TMPDIR"

if [[ "${1:-}" == "--print-temp-dir" ]]; then
    printf '%s\n' "$FS_EROFS_TEST_TMPDIR"
    exit 0
fi

cargo test "$@"
