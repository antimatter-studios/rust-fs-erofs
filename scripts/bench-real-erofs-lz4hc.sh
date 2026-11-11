#!/usr/bin/env bash
# Rebuild the pinned Android EROFS partition with Rust and liblz4 backends.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="${1:-$REPO/tests/fixtures/android-system_dlkm.img}"
OUT="${2:-$REPO/tmp/real-erofs-lz4hc}"
SAMPLES="${3:-5}"

fail() { echo "bench-real-erofs-lz4hc: $*" >&2; exit 1; }

[[ "$SAMPLES" =~ ^[0-9]+$ ]] && (( SAMPLES >= 3 )) || fail "SAMPLES must be an integer of at least 3"
[ -f "$FIXTURE" ] || fail "missing Android fixture: run tests/fixtures/download-android-erofs.sh"
command -v fsck.erofs >/dev/null || fail "fsck.erofs is required as the output oracle"
command -v jq >/dev/null || fail "jq is required to locate the compiled benchmark"
[ -x /usr/bin/time ] || fail "/usr/bin/time is required to measure peak RSS"

mkdir -p "$OUT"
SOURCE="$OUT/source"
if [ -e "$SOURCE" ]; then sudo rm -rf "$SOURCE"; fi
mkdir -p "$SOURCE"
sudo fsck.erofs --extract="$SOURCE" --preserve "$FIXTURE"

SOURCE_MANIFEST="$OUT/source.manifest"
sudo python3 "$REPO/tests/scripts/android-tree-manifest.py" "$SOURCE" > "$SOURCE_MANIFEST"
SOURCE_SHA256="$(sha256sum "$FIXTURE" | cut -d' ' -f1)"
EROFS_REV="$(git -C "$REPO" rev-parse --short HEAD)"
METADATA="$(cargo metadata --locked --format-version 1 --features bench-liblz4-backend)"
# The LZ4HC encoder is the am-lz4-flex package (#269): its version, and its
# commit when it comes from git rather than crates.io.
LZ4_FLEX_REV="$(jq -r '[.packages[] | select(.name == "am-lz4-flex")
    | "am-lz4-flex@\(.version)" + (if (.source // "") | startswith("git+") then "#" + (.source | split("#")[-1]) else "" end)]
    | if length == 1 then .[0] else empty end' <<< "$METADATA")"
[ -n "$LZ4_FLEX_REV" ] || fail "could not find exactly one am-lz4-flex package in Cargo metadata"
LZZZZ_MANIFEST="$(jq -r '.packages[] | select(.name == "lzzzz" and .version == "2.0.0") | .manifest_path' <<< "$METADATA")"
[ -n "$LZZZZ_MANIFEST" ] && [ "$LZZZZ_MANIFEST" != "null" ] || fail "could not locate lzzzz 2.0.0"
LIBLZ4_DIR="$(dirname "$LZZZZ_MANIFEST")/vendor/liblz4"
LIBLZ4_VERSION="$(sed -n 's/^#define LZ4_VERSION_MAJOR[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$LIBLZ4_DIR/lz4.h").$(sed -n 's/^#define LZ4_VERSION_MINOR[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$LIBLZ4_DIR/lz4.h").$(sed -n 's/^#define LZ4_VERSION_RELEASE[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$LIBLZ4_DIR/lz4.h")"
CPU_MODEL="$(LC_ALL=C lscpu | sed -n 's/^Model name:[[:space:]]*//p' | head -n 1)"
if [ -z "$CPU_MODEL" ]; then
    CPU_MODEL="$(awk -F: '/^model name[[:space:]]*:/ {sub(/^[[:space:]]+/, "", $2); print $2; exit}' /proc/cpuinfo)"
fi
[ -n "$CPU_MODEL" ] || CPU_MODEL="$(uname -m)"

REPORT="$OUT/benchmark.txt"
{
    echo "source_image_sha256=$SOURCE_SHA256"
    echo "erofs_revision=$EROFS_REV"
    echo "lz4_flex_revision=$LZ4_FLEX_REV"
    echo "liblz4_version=$LIBLZ4_VERSION"
    echo "lzzzz_version=2.0.0"
    echo "runner=$(uname -srm)"
    echo "rustc=$(rustc --version)"
    echo "cpu_model=$CPU_MODEL"
    echo "samples=$SAMPLES"
    echo "sample_order=alternating"
} > "$REPORT"

build_bench() {
    local target_dir="$1" features="$2" log="$3" json="$4"
    cargo bench --locked --no-default-features --features "$features" \
        --bench erofs_real_image --target-dir "$target_dir" --no-run \
        --message-format=json > "$json" 2> "$log"
    jq -r 'select(.reason == "compiler-artifact" and .target.name == "erofs_real_image") | .executable // empty' "$json" | tail -n 1
}

RUST_TARGET="$OUT/target-rust"
C_TARGET="$OUT/target-liblz4"
RUST_EXE="$(build_bench "$RUST_TARGET" lz4hc-workspace "$OUT/build-rust.log" "$OUT/build-rust.json")"
C_EXE="$(build_bench "$C_TARGET" bench-liblz4-backend "$OUT/build-liblz4.log" "$OUT/build-liblz4.json")"
[ -x "$RUST_EXE" ] || fail "Rust benchmark executable was not produced"
[ -x "$C_EXE" ] || fail "liblz4 benchmark executable was not produced"

run_sample() {
    local codec="$1" executable="$2" sample="$3"
    local image="$OUT/$codec-$sample.erofs" extracted="$OUT/$codec-$sample-extracted"
    local time_log="$OUT/$codec-$sample.time" output_log="$OUT/$codec-$sample.log"
    sudo rm -rf "$extracted"
    {
        sudo env LZ4_FLEX_REV="$LZ4_FLEX_REV" EROFS_REV="$EROFS_REV" \
            /usr/bin/time -f 'peak_rss_kib=%M' -o "$time_log" \
            "$executable" --input-dir "$SOURCE" --output "$image"
        cat "$time_log"
        echo "oracle_begin,codec=$codec,sample=$sample"
        sudo fsck.erofs --extract="$extracted" --preserve "$image"
        sudo python3 "$REPO/tests/scripts/android-tree-manifest.py" "$extracted" > "$OUT/$codec-$sample.manifest"
        diff -u "$SOURCE_MANIFEST" "$OUT/$codec-$sample.manifest"
        echo "oracle_pass,codec=$codec,sample=$sample,metadata_and_contents_match=true,image_bytes=$(stat -c %s "$image")"
    } | tee -a "$REPORT" > "$output_log"
    sudo rm -rf "$image" "$extracted"
    rm -f "$OUT/$codec-$sample.manifest"
}

for sample in $(seq 1 "$SAMPLES"); do
    if (( sample % 2 == 1 )); then
        run_sample liblz4 "$C_EXE" "$sample"
        run_sample lz4_flex "$RUST_EXE" "$sample"
    else
        run_sample lz4_flex "$RUST_EXE" "$sample"
        run_sample liblz4 "$C_EXE" "$sample"
    fi
done

cat "$REPORT"
