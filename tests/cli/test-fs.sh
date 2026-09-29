# fs.erofs on an image our own mkfs.erofs builds: ls of every directory
# matches the source tree, read of every file is the source byte for byte,
# get/info carry the canonical keys, the verbs a read-only format refuses
# answer so, and damaged images are refused with a structured error and
# nothing on stdout.
source "$(dirname "$0")/lib.sh"

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

img="$SANDBOX/fs.img"
mkfs.erofs -q "$img" "$src" >/dev/null 2>"$SANDBOX/mkfs.err"
check "mkfs.erofs made the image ($(cat "$SANDBOX/mkfs.err"))" test -s "$img"

# ls of every directory: the source's names, types and sizes.
listing_ok=1
while IFS= read -r d; do
    rel="${d#"$src"}"
    fs.erofs "$img" ls "${rel:-/}" >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err" || { listing_ok=0; fail "ls ${rel:-/}: $(cat "$SANDBOX/ls.err")"; continue; }
    want="$(cd "$d" && for n in * ; do [ -e "$n" ] || continue; if [ -d "$n" ]; then printf '%s dir\n' "$n"; else printf '%s file %s\n' "$n" "$(wc -c <"$n" | tr -d ' ')"; fi; done | LC_ALL=C sort)"
    got="$(jq -r '.[] | if .type == "dir" then "\(.name) dir" else "\(.name) \(.type) \(.size)" end' "$SANDBOX/ls.json" | LC_ALL=C sort)"
    [ "$want" = "$got" ] || { listing_ok=0; fail "ls ${rel:-/} lists $(echo $got | head -c 300), not $(echo $want | head -c 300)"; }
done < <(find "$src" -type d)
check "ls of every directory matches the source tree" test "$listing_ok" = 1
fs.erofs "$img" ls / >"$SANDBOX/root.json" 2>/dev/null
jq_check "every entry is typed" \
    'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number")' \
    "$SANDBOX/root.json"

# read of every file: the source, byte for byte.
read_ok=1
while IFS= read -r f; do
    rel="${f#"$src"}"
    fs.erofs "$img" read "$rel" >"$SANDBOX/read.out" 2>"$SANDBOX/read.err" || { read_ok=0; fail "read $rel: $(cat "$SANDBOX/read.err")"; continue; }
    cmp -s "$f" "$SANDBOX/read.out" || { read_ok=0; fail "read $rel differs from the source"; }
done < <(find "$src" -type f)
check "read of every file is the source byte for byte" test "$read_ok" = 1
fs.erofs "$img" read /big -o "$SANDBOX/big.out" >"$SANDBOX/o.out" 2>/dev/null
check "read -o exits 0 and prints nothing" test $? -eq 0 -a ! -s "$SANDBOX/o.out"
check "read -o wrote the file" cmp -s "$src/big" "$SANDBOX/big.out"

# get / info: every canonical key, typed; get and info identical.
fs.erofs "$img" get >"$SANDBOX/get.json" 2>/dev/null
check "get exits 0" test $? -eq 0
jq_check "get carries every canonical key with its type" \
    '(.fs=="erofs") and ((.label|type)=="string" or .label==null) and (.total_bytes|type)=="number" and .free_bytes==0 and .block_size==4096 and .dirty==false and (.erofs|type)=="object"' \
    "$SANDBOX/get.json"
jq_check "the inode count is the source's files and directories" \
    ".erofs.inode_count == $(find "$src" | wc -l | tr -d ' ')" "$SANDBOX/get.json"
check "total_bytes is the image's size" \
    test "$(jq .total_bytes "$SANDBOX/get.json")" = "$(wc -c <"$img" | tr -d ' ')"
fs.erofs "$img" info >"$SANDBOX/info.json" 2>/dev/null
check "info and get print the same" cmp -s "$SANDBOX/get.json" "$SANDBOX/info.json"
check "get erofs.uuid --text is a UUID" \
    grep -qE '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' <<<"$(fs.erofs "$img" get erofs.uuid --text)"

# The verbs a read-only format refuses: status 3, nothing on stdout.
for verb in "write /new" "mkdir /newdir" "set label X" "resize 1G --force"; do
    # shellcheck disable=SC2086  # the words are the point
    fs.erofs "$img" $verb </dev/null >"$SANDBOX/ro.out" 2>"$SANDBOX/ro.err"
    check "$verb exits 3" test $? -eq 3
    check "$verb prints nothing on stdout" test ! -s "$SANDBOX/ro.out"
    jq_check "$verb says EROFS is read-only" '.code == 3 and (.error | test("EROFS is read-only"))' "$SANDBOX/ro.err"
done

# --offset: the image embedded a MiB into a larger file.
{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
fs.erofs --offset 1048576 "$SANDBOX/embedded.img" read /one >"$SANDBOX/off.out" 2>/dev/null
check "--offset reaches the embedded image" cmp -s "$src/one" "$SANDBOX/off.out"

# Failures: status 1, a structured error, nothing on stdout -- never a panic.
refuse() {
    local what="$1"
    shift
    "$@" >"$SANDBOX/f.out" 2>"$SANDBOX/f.err"
    check "$what exits 1 (not a panic)" test $? -eq 1
    check "$what prints nothing on stdout" test ! -s "$SANDBOX/f.out"
    jq_check "$what is a structured error" '.code == 1 and (.error|type) == "string"' "$SANDBOX/f.err"
}
refuse "read of a directory" fs.erofs "$img" read /a
refuse "ls of a missing path" fs.erofs "$img" ls /missing
refuse "a missing image" fs.erofs "$SANDBOX/absent.img" ls /
cp "$img" "$SANDBOX/csum.img"
printf '\377' | dd of="$SANDBOX/csum.img" bs=1 seek=$(( 1024 + 48 )) conv=notrunc 2>/dev/null
refuse "a bad superblock checksum" fs.erofs "$SANDBOX/csum.img" get
head -c 2048 "$img" >"$SANDBOX/cut.img"
refuse "a truncated image" fs.erofs "$SANDBOX/cut.img" ls /
refuse "a file read from a truncated image" fs.erofs "$SANDBOX/cut.img" read /big

finish
