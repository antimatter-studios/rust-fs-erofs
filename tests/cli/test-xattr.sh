# Empty sets and refusals on the installed tool. Populated sets and raw
# names/values are checked against the guest kernel in cli_xattr_kernel.rs.
source "$(dirname "$0")/lib.sh"

mkdir -p "$SANDBOX/src/dir"
printf content >"$SANDBOX/src/file"
img="$SANDBOX/xattr.img"
mkfs.erofs -q "$img" "$SANDBOX/src" >"$SANDBOX/out" 2>"$SANDBOX/err"
check 'mkfs made the empty-xattr fixture' test $? -eq 0
cp "$img" "$SANDBOX/before.img"
for node in / /file /dir; do
    fs.erofs "$img" xattr "$node" >"$SANDBOX/list" 2>"$SANDBOX/err"
    check "xattr $node exits 0" test $? -eq 0
    jq_check "xattr $node has no attributes" '.==[]' "$SANDBOX/list"
    check "xattr $node has no error" test ! -s "$SANDBOX/err"
done
fs.erofs "$img" xattr --text /file >"$SANDBOX/text" 2>"$SANDBOX/err"
check 'empty --text exits 0' test $? -eq 0
check 'empty --text is silent' test ! -s "$SANDBOX/text"

for args in '/file user.missing' '/missing' '/missing user.missing'; do
    # shellcheck disable=SC2086 # Each case is a fixed sequence of operands.
    fs.erofs "$img" xattr $args >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "xattr $args exits 1" test $? -eq 1
    check "xattr $args has no stdout" test ! -s "$SANDBOX/out"
    jq_check "xattr $args has a structured error" '.code==1 and (.error|type)=="string"' "$SANDBOX/err"
done
fs.erofs "$img" xattr /file user.missing -o "$SANDBOX/value" >"$SANDBOX/out" 2>"$SANDBOX/err"
check 'missing attribute exits 1 with -o' test $? -eq 1
check 'missing attribute names itself' grep -q 'user.missing' "$SANDBOX/err"
check 'missing attribute creates no output file' test ! -e "$SANDBOX/value"
check 'missing attribute creates no partial file' test ! -e "$SANDBOX/value.partial"

for args in '' '/file -o value' '/file user.a extra'; do
    # shellcheck disable=SC2086
    fs.erofs "$img" xattr $args >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "invalid xattr $args exits 2" test $? -eq 2
    check 'usage error has no stdout' test ! -s "$SANDBOX/out"
    jq_check 'usage error is structured' '.code==2' "$SANDBOX/err"
done
fs.erofs "$img" xattr --help >"$SANDBOX/help" 2>/dev/null
check 'xattr help exits 0' test $? -eq 0
check 'xattr help has examples' grep -q 'Examples:' "$SANDBOX/help"
check 'xattr leaves image unchanged' cmp -s "$img" "$SANDBOX/before.img"

finish
