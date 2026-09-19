#!/usr/bin/env bash
# Rebuild fuzz/corpus from images upstream mkfs.erofs wrote.
#
# The seeds are real EROFS images and real structures cut out of them,
# not random bytes: a random byte string is refused by the magic number
# on the first line of every one of these decoders and never reaches the
# arithmetic underneath. A valid image with one field changed reaches
# all of it.
#
# One image per compression algorithm, because the algorithm id selects
# an entirely different decode path, plus a chunk-based one, because
# that is a different data layout again.
#
# Needs erofs-utils 1.9 or newer: `-b` arrived after 1.5, and lzma and
# deflate are not built into older packages. The committed corpus means
# this only has to run when the seeds are being regenerated.
#
# Usage: [MKFS_EROFS=/path/to/mkfs.erofs] scripts/make-fuzz-corpus.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkfs="${MKFS_EROFS:-mkfs.erofs}"
work="$(mktemp -d "${TMPDIR:-/tmp}/erofs-fuzz-corpus.XXXXXX")"
trap 'rm -rf "$work"' EXIT

command -v "$mkfs" >/dev/null 2>&1 || [ -x "$mkfs" ] || {
    echo "mkfs.erofs not found; set MKFS_EROFS to one from erofs-utils 1.9 or newer" >&2
    exit 1
}

# A tree small enough to commit five images of, and varied enough that
# each one has something of every shape in it: a file large enough to
# span blocks, one compressible enough to be worth compressing, one
# incompressible so a pcluster is stored rather than compressed, a
# symlink, and a subdirectory so the root is not the only directory.
tree="$work/tree"
mkdir -p "$tree/sub"
head -c 8000 /dev/urandom > "$tree/random.bin"
python3 -c "import sys; open(sys.argv[1],'w').write('the quick brown fox jumps over the lazy dog. ' * 280)" "$tree/text.txt"
python3 -c "import sys; open(sys.argv[1],'w').write('x' * 5000)" "$tree/runs.txt"
echo "deep" > "$tree/sub/deep.txt"
ln -sf sub/deep.txt "$tree/link"

# Extended attributes, on the root directory itself so the root inode --
# the one every seed below is cut from -- carries an inline xattr area.
# Several, and of different lengths, because the area is a run of
# variable-length entries and one entry exercises none of the walking.
# A short prefixed name, a long one, and a value long enough to spill
# past the first entry slot.
command -v setfattr >/dev/null || {
    echo "setfattr not found; install attr (Debian: apt-get install attr)" >&2
    exit 1
}
setfattr -n user.colour -v blue "$tree"
setfattr -n user.a-rather-longer-attribute-name -v "$(printf 'v%.0s' $(seq 1 120))" "$tree"
setfattr -n user.x -v y "$tree"
setfattr -n user.colour -v green "$tree/text.txt"
setfattr -n security.fuzz -v seed "$tree/sub/deep.txt" 2>/dev/null || true

# Enough entries that the root directory needs a whole block of its own.
# A small root fits inline behind its inode, and then the corpus has no
# directory block in it at all -- which is how the first version of this
# script produced a dir_block corpus of nothing.
# Empty, deliberately: an entry with content of its own costs a whole
# data block per file and took the committed corpus to 1.5 MB. An empty
# file is an inode and a directory entry, which is all this needs.
for i in $(seq 1 240); do
    : > "$tree/entry-$(printf '%03d' "$i")"
done

rm -rf "$here/fuzz/corpus"
mkdir -p "$here/fuzz/corpus"/{image,superblock,inode,dir_block,inline_xattrs}

build() {
    local name="$1"; shift
    local img="$here/fuzz/corpus/image/$name.img"
    "$mkfs" "$@" "$img" "$tree" >/dev/null 2>&1 || {
        echo "mkfs.erofs could not build the '$name' image -- erofs-utils too old?" >&2
        exit 1
    }
}

build plain
build lz4     -zlz4hc
build lzma    -zlzma
build deflate -zdeflate
build chunked --chunksize=4096

# Structures cut out of those images, for the targets that take one
# structure rather than a whole device.
python3 - "$here/fuzz/corpus" <<'PY'
import os, struct, sys

root = sys.argv[1]
SB_OFFSET = 1024
SB_LEN = 128
MAGIC = 0xE0F5E1E2

def cut(kind, name, data):
    with open(os.path.join(root, kind, name), 'wb') as f:
        f.write(data)

found_dir = 0
found_xattr = 0
for img_name in sorted(os.listdir(os.path.join(root, 'image'))):
    stem = img_name[:-len('.img')]
    img = open(os.path.join(root, 'image', img_name), 'rb').read()

    sb = img[SB_OFFSET:SB_OFFSET + SB_LEN]
    magic, = struct.unpack_from('<I', sb, 0)
    assert magic == MAGIC, f"{img_name}: superblock magic is {magic:#x}"
    cut('superblock', f'{stem}.bin', sb)

    blkszbits = sb[12]
    blocksize = 1 << blkszbits
    root_nid, = struct.unpack_from('<H', sb, 14)
    meta_blkaddr, = struct.unpack_from('<I', sb, 40)

    # iloc: the inode table starts at meta_blkaddr, and a nid is an
    # index in 32-byte slots. 64 bytes rather than 32 so an extended
    # inode is whole, and the inline area behind a compact one is
    # included -- which is where the xattrs live.
    iloc = meta_blkaddr * blocksize + root_nid * 32
    cut('inode', f'{stem}-root.bin', img[iloc:iloc + 64])

    # The inline xattr area behind the root inode. Its length is
    # derived the way the format derives it: an icount of zero means no
    # area at all, and otherwise the count is in 4-byte units after a
    # 12-byte header.
    i_format, = struct.unpack_from('<H', img, iloc)
    xattr_icount, = struct.unpack_from('<H', img, iloc + 2)
    inode_size = 64 if (i_format & 1) else 32
    if xattr_icount:
        xattr_len = (xattr_icount - 1) * 4 + 12
        start = iloc + inode_size
        cut('inline_xattrs', f'{stem}-root.bin', img[start:start + xattr_len])
        found_xattr += 1

    # A directory block, found by its own shape rather than by walking
    # the inode. An erofs_dirent is 12 bytes -- a 64-bit nid, then
    # nameoff as a LITTLE-ENDIAN 16-BIT field at offset 8, then the file
    # type -- so the first entry's nameoff is the size of the dirent
    # array and a multiple of 12, and the names begin right after it.
    # Checking the second entry's nameoff lands after the first rules
    # out a block that merely happens to start with the right number.
    for off in range(0, len(img) - blocksize + 1, blocksize):
        block = img[off:off + blocksize]
        nameoff, = struct.unpack_from('<H', block, 8)
        if nameoff == 0 or nameoff % 12 != 0 or nameoff > blocksize:
            continue
        count = nameoff // 12
        if count < 2 or count * 12 > blocksize:
            continue
        second, = struct.unpack_from('<H', block, 12 + 8)
        if not (nameoff <= second <= blocksize):
            continue
        cut('dir_block', f'{stem}-block{off // blocksize}.bin', block)
        found_dir += 1
        break

assert found_dir, "no directory block found in any image -- the heuristic needs revisiting"
assert found_xattr, "no inline xattr area found -- did setfattr silently do nothing?"
PY

echo "corpus rebuilt under fuzz/corpus:"
find "$here/fuzz/corpus" -type f | sort | sed "s#$here/##"
echo "total: $(find "$here/fuzz/corpus" -type f | wc -l) seeds, $(du -sh "$here/fuzz/corpus" | cut -f1)"
