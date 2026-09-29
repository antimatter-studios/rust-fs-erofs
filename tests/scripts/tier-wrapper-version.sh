#!/usr/bin/env bash
# scripts/tier.sh refuses a budget wrapper that does not answer the contract.
#
# tier.sh runs every tier through rust-fs-core's scripts/output-budget.sh,
# found through `cargo metadata`. It checked the file EXISTS and nothing
# more, so any script at that path -- a core too old, a core too new, a
# half-finished edit in the developer's own ../rust-fs-core -- was trusted
# with deciding whether a tier passed. The contract core publishes is
# `output-budget.sh --version`, answering exactly
# `rust-fs-core-output-budget 1` (#131). A present-but-wrong copy is FATAL,
# and says so: "core is broken" reported as a green tier, or as "core is
# missing", is the quieter and more confusing failure.
#
# So this builds a throwaway crate whose am-fs-core is a fake with a wrapper
# of our choosing, runs the real tier.sh against it, and requires a refusal
# for a wrong or missing answer and a run for the right one.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); printf '  FAIL  %s\n' "$1"; }

printf 'tier-wrapper-version\n'

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
crate="$sandbox/crate"
core="$sandbox/core"
mkdir -p "$crate/scripts" "$crate/src" "$core/scripts" "$core/src"
cp scripts/tier.sh "$crate/scripts/"
: > "$crate/src/lib.rs"
: > "$core/src/lib.rs"
cat > "$crate/Cargo.toml" <<'TOML'
[package]
name = "tier-wrapper-probe"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
am-fs-core = { path = "../core" }
TOML
cat > "$core/Cargo.toml" <<'TOML'
[package]
name = "am-fs-core"
version = "0.0.0"
edition = "2021"
publish = false
TOML
if ! (cd "$crate" && cargo generate-lockfile --offline > /dev/null 2>&1); then
    bad "cargo could not lock the probe crate; this test needs cargo on PATH"
fi

# A wrapper that runs the command and prints a verdict, answering --version
# with whatever $1 is (nothing at all when $1 is empty).
fake_wrapper() {
    {
        printf '#!/usr/bin/env bash\n'
        if [ -n "$1" ]; then
            printf '[ "${1:-}" = --version ] && { echo "%s"; exit 0; }\n' "$1"
        else
            printf '[ "${1:-}" = --version ] && { echo "unknown argument" >&2; exit 2; }\n'
        fi
        printf 'while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done; shift\n'
        printf '"$@" > /dev/null 2>&1; s=$?; echo "fake: ran"; exit $s\n'
    } > "$core/scripts/output-budget.sh"
}

run_tier() {
    (cd "$crate" && bash scripts/tier.sh probe probe 10 1000 -- true 2>&1)
}

fake_wrapper "rust-fs-core-output-budget 1"
out="$(run_tier)"; status=$?
if [ "$status" -eq 0 ]; then ok; else
    bad "a wrapper answering the contract was refused (exit $status): $out"
fi

for answer in "rust-fs-core-output-budget 2" "something-else 1" ""; do
    fake_wrapper "$answer"
    out="$(run_tier)"; status=$?
    if [ "$status" -ne 0 ] && [ "${out#*fake: ran}" = "$out" ]; then ok; else
        bad "a wrapper answering --version with '${answer:-nothing}' ran the tier (exit $status): $out"
    fi
    case "$out" in
        *rust-fs-core-output-budget\ 1*) ok ;;
        *) bad "the refusal for '${answer:-nothing}' does not name the answer it wanted: $out" ;;
    esac
done

if [ -z "$(find "$crate/tmp" -name 'output-budget.*' 2>/dev/null)" ]; then ok; else
    bad "tier.sh left its copy of the wrapper behind in tmp/"
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
