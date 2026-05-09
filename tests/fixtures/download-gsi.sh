#!/usr/bin/env bash
# Download a real Android Generic System Image (GSI) for use as an
# end-to-end EROFS test fixture. The resulting `system.img` is a raw
# (non-sparse) EROFS image that our reader can open directly.
#
# Usage:
#   tests/fixtures/download-gsi.sh [DEST_PATH]
#
# Environment overrides:
#   GSI_URL          override the download URL
#   GSI_EXPECTED_SHA override the expected SHA256 of the downloaded zip
#   GSI_KEEP_ZIP     if set to "1", keep the downloaded .zip after unpack
#
# License posture:
#   Android GSIs are distributed by Google under the AOSP terms (the
#   userspace is Apache-2.0; kernel parts are GPL-2 but we don't extract
#   them). We treat the image as an opaque black-box test fixture and do
#   NOT redistribute it -- this script just curls a public URL.
#
# Reproducibility:
#   The default URL is pinned to a specific Android 17 (Beta) ARM64 GSI
#   release with a known SHA256. If Google rotates the URL, override
#   GSI_URL + GSI_EXPECTED_SHA to a current release from
#   https://developer.android.com/topic/generic-system-image/releases.

set -euo pipefail

# Defaults: Android 17 (Beta), aosp_arm64, build CP21.260330.008.
# Verified live on 2026-05-09 (HTTP 200, ~954 MB ZIP).
GSI_URL="${GSI_URL:-https://dl.google.com/developers/android/cinnamonbun/images/gsi/aosp_arm64-exp-CP21.260330.008-15199860-19fc67b1.zip}"
EXPECTED_SHA="${GSI_EXPECTED_SHA:-19fc67b172288b67d23aad1c6639460ad32e21ebe470a21f7dffc55aca54e11b}"

DEST="${1:-tests/fixtures/system.img}"

# ---- helpers ----------------------------------------------------------

die() {
    echo "error: $*" >&2
    exit 1
}

note() { echo "[gsi] $*"; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "neither sha256sum nor shasum found on PATH"
    fi
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"
}

# ---- preflight --------------------------------------------------------

need_cmd curl
need_cmd unzip

# `simg2img` is only required if the extracted system.img turns out to
# be a sparse-image-wrapped variant. Probe later, fail loudly with
# install hint at that point so users without the Android platform
# tools aren't blocked unless they actually need them.

# ---- download ---------------------------------------------------------

DEST_DIR="$(dirname "$DEST")"
mkdir -p "$DEST_DIR"

TMPDIR_HOLD="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_HOLD"' EXIT

ZIP_PATH="$TMPDIR_HOLD/gsi.zip"
note "downloading $GSI_URL"
note "  -> $ZIP_PATH"
curl -L --fail --progress-bar -o "$ZIP_PATH" "$GSI_URL"

# ---- verify -----------------------------------------------------------

note "verifying SHA256"
GOT_SHA="$(sha256_of "$ZIP_PATH")"
if [ "$GOT_SHA" != "$EXPECTED_SHA" ]; then
    die "SHA256 mismatch:
  expected: $EXPECTED_SHA
  got:      $GOT_SHA
The pinned URL may have rotated. Check
  https://developer.android.com/topic/generic-system-image/releases
and re-run with GSI_URL=... GSI_EXPECTED_SHA=..."
fi

# ---- unpack -----------------------------------------------------------

note "unzipping"
unzip -q -o "$ZIP_PATH" -d "$TMPDIR_HOLD/unpacked"

# Locate the system.img inside the unpacked tree. The zip layout has
# varied across Android versions; just find any system.img.
SRC_IMG="$(find "$TMPDIR_HOLD/unpacked" -type f -name 'system.img' -print -quit || true)"
[ -n "${SRC_IMG:-}" ] || die "no system.img found inside the GSI zip"

# ---- sparse-unwrap if needed -----------------------------------------
#
# Android factory images are usually stored as Android-sparse images.
# Detect the magic and run simg2img if present. If not present, give a
# clear install hint -- without simg2img we cannot proceed.

is_sparse_image() {
    # Android sparse magic: 0x3aff26ed (LE) at offset 0.
    # Read the first 4 bytes and compare hex.
    local hdr
    hdr="$(od -An -N4 -tx1 "$1" | tr -d ' \n')"
    [ "$hdr" = "ed26ff3a" ]
}

if is_sparse_image "$SRC_IMG"; then
    note "system.img is Android-sparse; unwrapping with simg2img"
    if ! command -v simg2img >/dev/null 2>&1; then
        die "simg2img not on PATH -- needed to unwrap a sparse system.img.
Install via: brew install android-platform-tools
(or your distro's android-tools package)"
    fi
    UNWRAPPED="$TMPDIR_HOLD/system.raw.img"
    simg2img "$SRC_IMG" "$UNWRAPPED"
    SRC_IMG="$UNWRAPPED"
else
    note "system.img is already raw (no sparse unwrap needed)"
fi

# ---- install ----------------------------------------------------------

mv -f "$SRC_IMG" "$DEST"
SIZE_BYTES="$(wc -c < "$DEST" | tr -d ' ')"
note "OK: $DEST ($SIZE_BYTES bytes)"

if [ "${GSI_KEEP_ZIP:-0}" = "1" ]; then
    KEEP_DEST="$DEST_DIR/gsi.zip"
    cp -f "$ZIP_PATH" "$KEEP_DEST"
    note "kept zip at $KEEP_DEST"
fi
