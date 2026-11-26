# mkfs.erofs: builds an image from a source tree, reports it as JSON read
# back from the image, writes symlinks as links, compresses when -z asks, and
# fails with a structured error on stderr and nothing on stdout.
source "$(dirname "$0")/lib.sh"

# The source tree: every size boundary a 4 KiB block has, a MiB of noise,
# a long name, deep nesting, a directory large enough to span blocks, and
# a symlink.
src="$SANDBOX/src"
mkdir -p "$src/a/b/c/d/e/f/g/h" "$src/wide"
: >"$src/empty"
printf 'x' >"$src/one"
head -c 4096 /dev/urandom >"$src/f4096"
head -c 4097 /dev/urandom >"$src/f4097"
head -c 1048576 /dev/urandom >"$src/big"
long="$(printf 'n%.0s' $(seq 1 255))"
printf 'long' >"$src/$long"
printf 'deep' >"$src/a/b/c/d/e/f/g/h/deep"
for i in $(seq 1 300); do printf '%s' "$i" >"$src/wide/entry-with-a-longish-name-$i"; done
ln -s one "$src/link"
files=$(( 5 + 1 + 1 + 300 ))
dirs=$(( 1 + 8 + 1 ))

img="$SANDBOX/out.img"
mkfs.erofs "$img" "$src" >"$SANDBOX/mkfs.json" 2>"$SANDBOX/mkfs.err"
status=$?
check "mkfs.erofs exits 0 ($(tail -n 1 "$SANDBOX/mkfs.err"))" test "$status" -eq 0
jq_check "the report's numbers are numbers" \
    '[.bytes, .block_size, .blocks, .inodes, .files, .directories] | all(type == "number")' \
    "$SANDBOX/mkfs.json"
jq_check "the report names the image and a 4096-byte block" \
    ".image == \"$img\" and .block_size == 4096" "$SANDBOX/mkfs.json"
check "the report's byte count is the image's size" \
    test "$(jq .bytes "$SANDBOX/mkfs.json")" = "$(wc -c <"$img" | tr -d ' ')"
jq_check "the report counts $files files and $dirs directories" \
    ".files == $files and .directories == $dirs" "$SANDBOX/mkfs.json"
jq_check "the inode count covers every file, directory and the symlink" \
    ".inodes == $(( files + dirs + 1 ))" "$SANDBOX/mkfs.json"
jq_check "the symlink is written, and nothing is left out" \
    '.symlinks == 1 and .skipped == []' "$SANDBOX/mkfs.json"
jq_check "the UUID is 8-4-4-4-12 hex" \
    '.uuid | test("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")' "$SANDBOX/mkfs.json"
fs.erofs "$img" stat /link >"$SANDBOX/link.json" 2>/dev/null
jq_check "the symlink reads back as a link to one" \
    '.type == "symlink" and .target == "one"' "$SANDBOX/link.json"

# The repository-named form is the same program, and the build is
# deterministic, so it writes the same bytes.
rust-fs-erofs mkfs -q "$SANDBOX/repo.img" "$src" >/dev/null 2>&1
check "rust-fs-erofs mkfs exits 0" test $? -eq 0
check "rust-fs-erofs mkfs writes the same image" cmp -s "$img" "$SANDBOX/repo.img"

# Every block size the format allows, and the report says which.
for bs in 512 1024 2048 4096 8192 16384 32768 65536; do
    mkfs.erofs -q -b "$bs" "$SANDBOX/bs.img" "$src" >"$SANDBOX/bs.json" 2>"$SANDBOX/bs.err"
    status=$?
    check "-b $bs exits 0 ($(cat "$SANDBOX/bs.err"))" test "$status" -eq 0
    jq_check "-b $bs is reported" ".block_size == $bs" "$SANDBOX/bs.json"
done
for bs in 256 3000 131072; do
    mkfs.erofs -b "$bs" "$SANDBOX/badbs.img" "$src" >"$SANDBOX/badbs.out" 2>"$SANDBOX/badbs.err"
    check "-b $bs exits 2" test $? -eq 2
    check "-b $bs prints nothing on stdout" test ! -s "$SANDBOX/badbs.out"
    check "-b $bs writes no image" test ! -e "$SANDBOX/badbs.img"
    jq_check "-b $bs is a structured usage error" '.code == 2 and (.error | test("power of 2"))' "$SANDBOX/badbs.err"
done

# --label: exit 3, not implemented, and nothing written.
mkfs.erofs -L BACKUP "$SANDBOX/label.img" "$src" >"$SANDBOX/label.out" 2>"$SANDBOX/label.err"
check "--label exits 3" test $? -eq 3
check "--label prints nothing on stdout" test ! -s "$SANDBOX/label.out"
check "--label writes no image" test ! -e "$SANDBOX/label.img"
jq_check "--label says not implemented" '.code == 3 and (.error | startswith("not implemented"))' "$SANDBOX/label.err"

# --text -q: nothing on stdout or stderr, as the tool always printed.
mkfs.erofs --text -q "$SANDBOX/text.img" "$src" >"$SANDBOX/text.out" 2>"$SANDBOX/text.err"
check "mkfs.erofs --text -q exits 0" test $? -eq 0
check "mkfs.erofs --text -q prints nothing on stdout" test ! -s "$SANDBOX/text.out"
check "mkfs.erofs --text -q prints nothing on stderr" test ! -s "$SANDBOX/text.err"

# -z: every compressor the tool has, spelled as erofs-utils spells it. The
# image is smaller and every file reads back.
zsrc="$SANDBOX/zsrc"
mkdir -p "$zsrc"
for i in $(seq 1 4000); do printf 'the quick brown fox jumps over the lazy dog\n'; done >"$zsrc/text"
head -c 65536 /dev/urandom >"$zsrc/noise"
mkfs.erofs -q "$SANDBOX/plain.img" "$zsrc" >/dev/null 2>&1
plain_bytes="$(wc -c <"$SANDBOX/plain.img" | tr -d ' ')"
for z in lz4 lz4hc lz4hc,12; do
    mkfs.erofs -q -z "$z" "$SANDBOX/z.img" "$zsrc" >"$SANDBOX/z.json" 2>"$SANDBOX/z.err"
    status=$?
    check "-z $z exits 0 ($(cat "$SANDBOX/z.err"))" test "$status" -eq 0
    named="$z"
    [ "$named" != lz4hc ] || named=lz4hc,9
    jq_check "-z $z is reported" ".compression == \"$named\"" "$SANDBOX/z.json"
    check "-z $z makes a smaller image" \
        test "$(wc -c <"$SANDBOX/z.img" | tr -d ' ')" -lt "$plain_bytes"
    read_ok=1
    for f in text noise; do
        fs.erofs "$SANDBOX/z.img" read "/$f" >"$SANDBOX/z.out" 2>/dev/null &&
            cmp -s "$zsrc/$f" "$SANDBOX/z.out" || read_ok=0
    done
    check "-z $z reads back every file" test "$read_ok" = 1
done
mkfs.erofs -z zstd "$SANDBOX/badz.img" "$src" >"$SANDBOX/badz.out" 2>"$SANDBOX/badz.err"
check "-z zstd exits 2" test $? -eq 2
check "-z zstd prints nothing on stdout" test ! -s "$SANDBOX/badz.out"
check "-z zstd writes no image" test ! -e "$SANDBOX/badz.img"
jq_check "-z zstd is a structured usage error" '.code == 2 and (.error | test("lz4"))' "$SANDBOX/badz.err"

# A wrong command line: status 2, a JSON error on stderr, empty stdout.
mkfs.erofs --no-such-flag "$img" "$src" >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
check "an unknown flag exits 2" test $? -eq 2
check "an unknown flag prints nothing on stdout" test ! -s "$SANDBOX/usage.out"
jq_check "an unknown flag is a structured error" '.code == 2 and (.error | test("no-such-flag"))' "$SANDBOX/usage.err"

# A failed run: status 1, the same shape, and no image.
mkfs.erofs "$SANDBOX/fail.img" "$SANDBOX/absent" >"$SANDBOX/fail.out" 2>"$SANDBOX/fail.err"
check "a missing source exits 1" test $? -eq 1
check "a missing source prints nothing on stdout" test ! -s "$SANDBOX/fail.out"
check "a missing source writes no image" test ! -e "$SANDBOX/fail.img"
jq_check "a missing source is a structured error" '.code == 1 and (.error | test("walking"))' "$SANDBOX/fail.err"

finish
