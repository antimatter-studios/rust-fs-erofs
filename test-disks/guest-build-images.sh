#!/usr/bin/env bash
#
# guest-build-images.sh OUT_DIR [name...] — build the test-disks/*.img
# fixtures. RUNS AS ROOT INSIDE THE fs-linux-test-harness VM; the host
# side is test-disks/build-fixtures.sh, which is what you run.
#
# WHY IN HERE. Every image is made by `mkfs.erofs`, and the only
# erofs-utils this project trusts is the 1.9.1 that scripts/vm-setup.sh
# builds from source in this guest — see that script for what a
# packaged build gets wrong. The images are ordinary files afterwards,
# the same bytes on every architecture, which is what lets the
# no-KVM arm64 CI job read them without a VM of its own.
#
# THE SOURCE TREE IS GENERATED, NOT CHECKED IN, and tests/fixture_matrix.rs
# regenerates the same bytes to compare against. Both sides use the LCG
# spelled out in `gen.py` below: x = x*1103515245 + 12345 (mod 2^32),
# byte = (x >> 16) & 0xff, seeded 0x12345678. A fixture whose expectation
# is a copy of what the reader produced proves nothing; a fixture whose
# expectation is a rule proves the reader read the rule.
#
# EVERY IMAGE IS REPRODUCIBLE: -T0 --all-time fixes every timestamp and
# -U fixes the UUID, so rebuilding produces the same bytes and a diff in
# the fixtures means a diff in the tool.
#
# WHAT IS NOT HERE. `-Efragments`, `-Eall-fragments` and `-Ededupe` are
# left out on purpose: this crate reads those layouts back as wrong
# bytes rather than as an error (#125). A fixture for a known defect
# belongs in the test that names the defect, not in a matrix whose job
# is to stay green.
set -euo pipefail

OUT="${1:?usage: guest-build-images.sh OUT_DIR [name...]}"
shift || true

# name:flags — one image per line. THIS LIST AND build-fixtures.sh's
# IMAGES MUST AGREE, and chores.yml's `generates:` names the same files.
SPECS=$(cat <<'EOF'
plain|-b4096
lz4|-b4096 -zlz4
lz4hc|-b4096 -zlz4hc
lzma|-b4096 -zlzma
deflate|-b4096 -zdeflate
zstd|-b4096 -zzstd
b1024|-b1024 -zlz4
b512|-b512
bigpcluster|-b4096 -zlz4 -C65536
ztailpacking|-b4096 -zlz4 -Eztailpacking
chunked|-b4096 --chunksize=65536
EOF
)

want=""
[ $# -gt 0 ] && want=" $* "

mkdir -p "$OUT"
work="$(mktemp -d /var/tmp/erofs-fixtures.XXXXXX)"
trap 'rm -rf "$work"' EXIT
src="$work/src"
mkdir -p "$src/sub/deeper"

python3 - "$src" <<'EOF'
import os, sys

root = sys.argv[1]

def lcg(n, seed=0x12345678):
    out = bytearray()
    x = seed
    for _ in range(n):
        x = (x * 1103515245 + 12345) & 0xFFFFFFFF
        out.append((x >> 16) & 0xFF)
    return bytes(out)

files = {
    "hello.txt": b"hello erofs\n",
    "empty.bin": b"",
    "compressible.txt": b"the same line over and over\n" * 2000,
    "incompressible.bin": lcg(16384),
    "sub/nested.txt": b"nested\n",
    "sub/deeper/leaf.bin": bytes([0x11]) * 5000,
}
for name, data in files.items():
    with open(os.path.join(root, name), "wb") as fh:
        fh.write(data)
os.symlink("hello.txt", os.path.join(root, "link"))
EOF

built=0
printf '%s\n' "$SPECS" | while IFS='|' read -r name flags; do
    [ -n "$name" ] || continue
    if [ -n "$want" ]; then
        case "$want" in *" $name "*) ;; *) continue ;; esac
    fi
    img="$OUT/erofs-$name.img"
    rm -f "$img"
    # shellcheck disable=SC2086  # the flags are meant to split into words
    mkfs.erofs -T0 --all-time -U 6f5ad6b1-3dfe-4e4c-9c1b-9c7c6f0b1a22 \
        $flags "$img" "$src" >/dev/null
    # fsck.erofs's clean exit is the tool's own verdict on what it just
    # wrote. A fixture the writer's own checker rejects is not a fixture.
    fsck.erofs "$img" >/dev/null
    echo "built erofs-$name.img ($flags)"
    built=$((built + 1))
done
