#!/usr/bin/env bash
# No features.d/ fragment deletes a row of docs/features.md.
#
# rust-fs-core's fragment format has `remove: KEY`, which drops a row when a
# release gathers it. This repository never uses it: a feature that goes
# away keeps its row in the Removed state, saying when and why, because a
# deleted row reads exactly like a feature that never existed and hides a
# removal that was really a regression (AGENTS.md, "Keep the features page
# current").
#
# Usage: features-keep-rows.sh [DIR]   (DIR defaults to features.d)
set -euo pipefail
dir="${1:-features.d}"
[ -d "$dir" ] || { echo "features-keep-rows: $dir is not a directory" >&2; exit 2; }
found="$(grep -nE '^remove:' "$dir"/*.md 2>/dev/null || true)"
if [ -n "$found" ]; then
    echo "features-keep-rows: a fragment deletes a features-page row; set the row to Removed instead (AGENTS.md):" >&2
    printf '%s\n' "$found" >&2
    exit 1
fi
