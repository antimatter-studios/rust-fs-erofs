# mkfs.erofs: builds an image from a source tree, reports it as JSON read
# back from the image, names what it left out, and fails with a structured
# error on stderr and nothing on stdout.
source "$(dirname "$0")/lib.sh"

# The source tree: every size boundary a 4 KiB block has, a MiB of noise,
# a long name, deep nesting, a directory large enough to span blocks, and
# a symlink the builder leaves out.
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
check "mkfs.erofs exits 0 ($(tail -n 1 "$SANDBOX/mkfs.err"))" test $? -eq 0
jq_check "the report's numbers are numbers" \
    '[.bytes, .block_size, .blocks, .inodes, .files, .directories] | all(type == "number")' \
    "$SANDBOX/mkfs.json"
jq_check "the report names the image and a 4096-byte block" \
    ".image == \"$img\" and .block_size == 4096" "$SANDBOX/mkfs.json"
check "the report's byte count is the image's size" \
    test "$(jq .bytes "$SANDBOX/mkfs.json")" = "$(wc -c <"$img" | tr -d ' ')"
jq_check "the report counts $files files and $dirs directories" \
    ".files == $files and .directories == $dirs" "$SANDBOX/mkfs.json"
jq_check "the inode count covers every file and directory" \
    ".inodes == $(( files + dirs ))" "$SANDBOX/mkfs.json"
# Windows joins the walked directory and the name with a backslash, so a
# reported path is compared with its separators as slashes.
jq_check "the symlink is named as left out" \
    "(.skipped | map(.path |= (split(\"\\\\\") | join(\"/\")))) == [{\"path\": \"$src/link\", \"reason\": \"symlink\"}]" "$SANDBOX/mkfs.json"
jq_check "the UUID is 8-4-4-4-12 hex" \
    '.uuid | test("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")' "$SANDBOX/mkfs.json"
tr '\\' / <"$SANDBOX/mkfs.err" >"$SANDBOX/mkfs.err.slashed"
check "the symlink is warned about on stderr" grep -q "warning: left out $src/link (symlink)" "$SANDBOX/mkfs.err.slashed"

# The repository-named form is the same program, and the build is
# deterministic, so it writes the same bytes.
rust-fs-erofs mkfs -q "$SANDBOX/repo.img" "$src" >/dev/null 2>&1
check "rust-fs-erofs mkfs exits 0" test $? -eq 0
check "rust-fs-erofs mkfs writes the same image" cmp -s "$img" "$SANDBOX/repo.img"

# Every block size the format allows, and the report says which.
for bs in 512 1024 2048 4096 8192 16384 32768 65536; do
    mkfs.erofs -q -b "$bs" "$SANDBOX/bs.img" "$src" >"$SANDBOX/bs.json" 2>"$SANDBOX/bs.err"
    check "-b $bs exits 0 ($(cat "$SANDBOX/bs.err"))" test $? -eq 0
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
