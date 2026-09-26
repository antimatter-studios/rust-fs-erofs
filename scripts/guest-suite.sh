#!/usr/bin/env bash
#
# guest-suite.sh [cargo test args...] — THE WHOLE SUITE, INSIDE THE VM.
#
# The fs-linux-test-harness [test] guest_command: `chore test:vm` (and
# `chore test` on a host that is not Linux) boots the VM and runs this
# from /repo, where the harness mounts this repository.
#
# WHY IT EXISTS. We run the Linux tests on Linux. On a Linux host that is
# the host itself and this is not used. On a Mac there is no EROFS
# module, no loop mount and no erofs-utils worth trusting — so the suite
# runs in here, against the same sources, with the same pinned toolchain.
#
# The toolchain and the build directory live on the VM's own disk
# (/var/lib, /var/cache), which outlives `vm:down` and is thrown away by
# `vm:destroy`: the first run pays a full build, later runs are
# incremental. The repository itself is a 9p mount, so nothing is written
# back into it except what the tests write to tmp/.
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "guest-suite.sh runs INSIDE the harness VM ('chore test:vm')." >&2; exit 1; }

# The path dependencies in Cargo.toml, by directory name.
SIBLINGS="rust-fs-core"

RUST_ROOT=/var/lib/fs-erofs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"
export CARGO_TARGET_DIR=/var/cache/fs-erofs-target
export PATH="$CARGO_HOME/bin:$PATH"

# THE SIBLING CRATES. This crate's Cargo.toml has a path dependency on
# ../rust-fs-core, which on the host is a sibling checkout — and the
# guest is given this repository, not the directory that holds it. The
# harness mounts us at /repo, whose parent IS the guest's root, so
# `../rust-fs-core` resolves to /rust-fs-core: the task stages the
# sibling on the share (from its pinned, clean checkout) and this links
# it into place. `chore siblings` is what keeps it at the right ref.
# shellcheck disable=SC2043  # one sibling today; the list is the point
for sibling in $SIBLINGS; do
    staged="/share/siblings/$sibling"
    [ -d "$staged" ] || {
        echo "guest-suite.sh: $sibling is not staged on the share; 'chore test:vm' does that." >&2
        exit 1
    }
    [ -L "/$sibling" ] || ln -sfn "$staged" "/$sibling"
done

cd /repo
command -v cargo >/dev/null ||
    { echo "guest-suite.sh: no cargo in the guest — 'chore vm:provision' installs it." >&2; exit 1; }

# THE SAME SELECTION THE NATIVE PATH USES, which means `all` and NOT
# "every test there is".
#
# `test-targets.sh all` deliberately leaves out the GSI suite, because
# that one needs a ~2 GiB Android system image which is not
# redistributable, is gitignored, and is fetched by hand for `chore
# test:gsi`. Running bare `cargo test` in here instead picks oracle_gsi
# up, and since tests never skip on a missing fixture it FAILS -- so the
# `suite in the guest` CI job could only ever be red, on a machine that
# by design cannot have the fixture. Measured before this line existed:
#
#   test open_and_walk_gsi ... FAILED
#   tests/fixtures/system.img is missing. Fetch it with
#   tests/fixtures/download-gsi.sh ... Tests never skip on a missing fixture.
#
# The guest is a way of running the Linux suite somewhere else, not a
# different suite, so it takes its targets from the same place.
echo "== in-guest suite: $(uname -srm), $(cargo --version)"
started=$(date +%s)
# shellcheck disable=SC2046  # the words are the target list
scripts/test.sh --locked --release $(scripts/test-targets.sh all) "$@"
echo "== in-guest suite: $(( $(date +%s) - started ))s"
