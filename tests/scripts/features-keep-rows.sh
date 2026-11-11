#!/usr/bin/env bash
# scripts/features-keep-rows.sh passes fragments that add, replace, rename
# or mark a row Removed, and fails on one that deletes a row, naming it.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
guard="$here/scripts/features-keep-rows.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir "$work/ok" "$work/bad"
cat > "$work/ok/feat-a.md" <<'MD'
## Reading

| A feature | Removed: why; supported from 0.3.0 to 0.5.1 | Unreleased (#1) | #2 | |

rename: Old key
| New key | Supported | Unreleased (#1) | | |
MD
cp "$work/ok/feat-a.md" "$work/bad/feat-a.md"
printf '## Reading\n\nremove: A feature\n' > "$work/bad/fix-b.md"

bash "$guard" "$work/ok" || { echo "FAIL: a fragment that keeps every row was refused" >&2; exit 1; }
if bash "$guard" "$work/bad" 2>"$work/err"; then
    echo "FAIL: a fragment with remove: was accepted" >&2; exit 1
fi
grep -q 'fix-b.md:3:remove: A feature' "$work/err" || {
    echo "FAIL: the refusal does not name the fragment and line:" >&2; cat "$work/err" >&2; exit 1; }
bash "$guard" "$here/features.d" || { echo "FAIL: this repository's own fragments delete a row" >&2; exit 1; }
echo "ok: features-keep-rows refuses remove: and passes rows kept"
