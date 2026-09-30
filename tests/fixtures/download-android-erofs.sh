#!/usr/bin/env bash
#
# download-android-erofs.sh [DEST]   fetch the Android EROFS fixture
#                                    (default tests/fixtures/android-system_dlkm.img)
#
# A real EROFS image that neither this repository nor mkfs.erofs in a temp
# directory produced: the `system_dlkm` partition of Google's Android 16
# (API 36) aosp_atd arm64 emulator system image, as Google's own build
# wrote it. LZ4-compressed (98 of its 108 inodes), big pclusters, 0padding,
# a security.selinux label on every inode. 7,397,376 bytes. `chore
# test:android` (tests/oracle_android.rs) reads it and compares every
# inode against tests/fixtures/android-system_dlkm.manifest, which the
# Linux kernel's EROFS driver and fsck.erofs produced from the same bytes.
#
# WHY NOT THE GSI THIS USED TO FETCH: every Generic System Image on
# developer.android.com -- Android 16 and 17, aosp and gms, arm64 -- ships
# an ext4 system.img (#133, #54), so no pin of one could ever pass. Read
# off each zip's system.img superblock on 2026-09-30. The emulator image's
# system_dlkm is the one EROFS partition either image family has.
#
# WHY IT IS DOWNLOADED AND NOT COMMITTED: it is Google's, distributed under
# the Android SDK license, and holds kernel modules. This script fetches a
# public URL; nothing is redistributed from here. The manifest (paths,
# sizes, hashes, labels) is ours and is committed.
#
# HOW IT IS CUT OUT. The zip's arm64-v8a/system.img is a GPT disk whose
# `super` partition starts at LBA 4096; its dynamic-partition metadata puts
# system_dlkm 935,329,792 bytes into super. Those offsets belong to this
# pin, so they are pinned beside it rather than parsed, and the result is
# checked twice: the EROFS magic (a wrong offset or a wrong pin names the
# filesystem it found instead) and the partition's SHA-256.
#
# Environment:
#   ANDROID_EROFS_ZIP   use this local copy of the zip instead of downloading
#   ANDROID_EROFS_KEEP_ZIP=1   keep the downloaded zip next to DEST
set -euo pipefail

ZIP_URL="https://dl.google.com/android/repository/sys-img/aosp_atd/arm64-v8a-36_r01.zip"
# SHA-1 is what Google's repository manifest (sys-img/aosp_atd/sys-img2-3.xml)
# publishes for the zip; the partition's SHA-256 below is the check that
# decides.
ZIP_SHA1="906703f87bd947048b9099b8fbf34bd8ee33c77e"
ZIP_BYTES=712011575
MEMBER="arm64-v8a/system.img"
OFFSET=$(( 4096 * 512 + 935329792 ))
LENGTH=7397376
SHA256="29c334adbab7d5a25cc26b63731d90f3ce8cd26c2bcacf2a7b23fff8475f640c"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEST="${1:-$REPO/tests/fixtures/android-system_dlkm.img}"

die() { echo "download-android-erofs: $*" >&2; exit 1; }
note() { echo "[android-erofs] $*"; }

hash_of() { # ALGO FILE
    if command -v "sha$1sum" >/dev/null 2>&1; then "sha$1sum" "$2" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then shasum -a "$1" "$2" | cut -d' ' -f1
    else die "neither sha$1sum nor shasum is on PATH"
    fi
}

# What filesystem does FILE start with? Named, so a wrong offset or a pin
# that moved says what it found rather than only that it is not EROFS.
fs_kind() {
    local erofs ext
    erofs="$(od -An -tx1 -j1024 -N4 "$1" | tr -d ' \n')"
    ext="$(od -An -tx1 -j1080 -N2 "$1" | tr -d ' \n')"
    if [ "$erofs" = e2e1f5e0 ]; then echo erofs
    elif [ "$ext" = 53ef ]; then echo ext4
    else echo "unknown (bytes 1024..1028 are '$erofs')"
    fi
}

command -v unzip >/dev/null 2>&1 || die "unzip is not on PATH"
mkdir -p "$(dirname "$DEST")"
work="$(mktemp -d "${TMPDIR:-/tmp}/android-erofs.XXXXXX")"
trap 'rm -rf "$work"' EXIT

zip="${ANDROID_EROFS_ZIP:-}"
if [ -z "$zip" ]; then
    command -v curl >/dev/null 2>&1 || die "curl is not on PATH"
    zip="$work/image.zip"
    note "downloading $ZIP_URL ($ZIP_BYTES bytes)"
    curl -fL --retry 3 --silent --show-error -o "$zip" "$ZIP_URL"
fi
[ -f "$zip" ] || die "$zip is not a file"

note "verifying the zip's SHA-1 against Google's repository manifest"
got="$(hash_of 1 "$zip")"
[ "$got" = "$ZIP_SHA1" ] || die "$zip has SHA-1 $got, expected $ZIP_SHA1"

note "cutting system_dlkm out of $MEMBER"
# `head` closes the pipe once it has LENGTH bytes, which ends unzip and
# tail with SIGPIPE; that is the expected way for this pipeline to stop, so
# its status is not the verdict. The length, the magic and the hash are.
set +o pipefail
unzip -p "$zip" "$MEMBER" | tail -c +"$(( OFFSET + 1 ))" | head -c "$LENGTH" > "$work/part.img"
set -o pipefail

size="$(wc -c < "$work/part.img" | tr -d ' ')"
[ "$size" = "$LENGTH" ] || die "cut $size bytes out of $MEMBER, expected $LENGTH"
kind="$(fs_kind "$work/part.img")"
[ "$kind" = erofs ] || die "the partition at byte $OFFSET of $MEMBER is $kind, not EROFS"
got="$(hash_of 256 "$work/part.img")"
[ "$got" = "$SHA256" ] || die "the partition has SHA-256 $got, expected $SHA256"

mv -f "$work/part.img" "$DEST"
if [ "${ANDROID_EROFS_KEEP_ZIP:-}" = 1 ] && [ -z "${ANDROID_EROFS_ZIP:-}" ]; then
    mv -f "$zip" "$(dirname "$DEST")/$(basename "$ZIP_URL")"
fi
note "OK: $DEST ($LENGTH bytes, EROFS, SHA-256 verified)"
