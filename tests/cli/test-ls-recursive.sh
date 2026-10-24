# Presentation and termination checks; cli_ls_recursive_kernel.rs supplies
# the independent mounted reference-image comparison.
source "$(dirname "$0")/lib.sh"

python3 - "$SANDBOX" <<'PY'
import pathlib, struct, sys
for fault in ('tree', 'cycle', 'duplicate', 'unreadable'):
    b = bytearray(8 * 4096)
    def put(off, fmt, *v):
        struct.pack_into('<' + fmt, b, off, *v)
    put(1024, 'I', 0xe0f5e1e2)
    put(1036, 'BBHQQIII', 12, 0, 1, 8, 1700000000, 0, 8, 1)
    def inode(nid, mode, size, block):
        put(4096 + nid * 32, 'HHHHIIIIHHI', 0, 0, mode, 1, size, 0, block, nid, 0, 0, 0)
    def directory(nid, block, entries):
        entries = [(b'.', nid, 2), (b'..', 1, 2)] + entries
        off = len(entries) * 12
        for i, (name, child, kind) in enumerate(entries):
            put(block * 4096 + i * 12, 'QHBB', child, off, kind, 0)
            b[block * 4096 + off:block * 4096 + off + len(name)] = name
            off += len(name)
        inode(nid, 0o40755, off, block)
    directory(1, 2, [(b'a', 1 if fault == 'cycle' else 3, 2), (b'empty', 7, 2),
                     (b'f', 11, 1), (b'link', 13, 7), (b'z', 3 if fault == 'duplicate' else 15, 2 if fault == 'duplicate' else 1)])
    directory(3, 3, [(b'deep', 5, 2)])
    directory(5, 4, [(b'file', 9, 1)])
    inode(7, 0o40755, 0, 0)
    for nid in (9, 11, 15):
        inode(nid, 0o100644, 3, 5)
    inode(13, 0o120777, 1, 6)
    b[5 * 4096:5 * 4096 + 3] = b'abc'
    b[6 * 4096] = ord('a')
    if fault == 'unreadable':
        put(4096 + 3 * 32 + 16, 'I', 99)
    pathlib.Path(sys.argv[1], fault + '.img').write_bytes(b)
PY
check "fixtures are built" test $? -eq 0
img="$SANDBOX/tree.img"
cp "$img" "$SANDBOX/before.img"
fs.erofs "$img" ls --recursive / >"$SANDBOX/tree.json" 2>"$SANDBOX/tree.err"
check "recursive ls exits 0" test $? -eq 0
check "successful recursion has no error" test ! -s "$SANDBOX/tree.err"
jq_check "sorted depth-first paths" '[.[].path]==["/a","/a/deep","/a/deep/file","/empty","/f","/link","/z"]' "$SANDBOX/tree.json"
jq_check "depth counts edges from the starting directory" '[.[].depth]==[1,2,3,1,1,1,1]' "$SANDBOX/tree.json"
jq_check "entries retain the ls fields" 'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number" and (has("revisited")|not))' "$SANDBOX/tree.json"
fs.erofs "$img" ls -R --max-depth 3 --limit 7 / >"$SANDBOX/again.json" 2>/dev/null
check "exact bounds succeed" test $? -eq 0
check "two runs print identical bytes" cmp -s "$SANDBOX/tree.json" "$SANDBOX/again.json"
for spec in 'max-depth 2 2 /a/deep/file' 'limit 4 4 /f' 'max-depth 0 0 /a' 'limit 0 0 /a'; do
    read -r bound value count stopped <<<"$spec"
    fs.erofs "$img" ls -R "--$bound" "$value" / >"$SANDBOX/partial.json" 2>"$SANDBOX/partial.err"
    check "$bound $value exits 1" test $? -eq 1
    check "$bound retains exactly the collected prefix" test "$(jq -c . "$SANDBOX/partial.json")" = "$(jq -c ".[0:$count]" "$SANDBOX/tree.json")"
    jq_check "$bound names the stopped path" ".code==1 and (.error|contains(\"--$bound\")) and (.error|contains(\"$stopped\"))" "$SANDBOX/partial.err"
done
for fault in cycle duplicate unreadable; do
    fs.erofs "$SANDBOX/$fault.img" ls -R / >"$SANDBOX/fault.json" 2>"$SANDBOX/fault.err"
    rc=$?
    if [ "$fault" = unreadable ]; then
        check "unreadable directory exits 1" test "$rc" -eq 1
        jq_check "unreadable directory retains its entry" 'length==1 and .[0].path=="/a"' "$SANDBOX/fault.json"
        jq_check "unreadable directory is named" '.code==1 and (.error|contains("/a"))' "$SANDBOX/fault.err"
    else
        check "$fault terminates successfully" test "$rc" -eq 0
        if [ "$fault" = cycle ]; then stopped=/a; else stopped=/z; fi
        jq_check "$fault lists but never enters a repeated directory" "any(.[]; .path==\"$stopped\" and .revisited==true) and all(.[]; (.path|startswith(\"$stopped/\"))|not)" "$SANDBOX/fault.json"
    fi
done
fs.erofs "$img" ls -R a//deep/ >"$SANDBOX/sub.json" 2>/dev/null
jq_check "subtree paths are absolute with relative depth" 'length==1 and .[0].path=="/a/deep/file" and .[0].depth==1' "$SANDBOX/sub.json"
fs.erofs "$img" ls -R /link >"$SANDBOX/link.json" 2>/dev/null
jq_check "starting symlink is not followed" 'length==1 and .[0].path=="/link" and .[0].depth==0 and .[0].type=="symlink" and .[0].target=="a"' "$SANDBOX/link.json"
fs.erofs "$img" ls -R --limit 0 --max-depth 0 /empty >"$SANDBOX/empty.json" 2>/dev/null
check "empty directory fits zero bounds" test $? -eq 0
jq_check "empty directory has no entries" '.==[]' "$SANDBOX/empty.json"
fs.erofs "$img" ls -R --text / >"$SANDBOX/tree.txt" 2>/dev/null
check "text uses full paths" grep -q ' /a/deep/file$' "$SANDBOX/tree.txt"
fs.erofs --text "$img" ls -R --json --limit 1 / >"$SANDBOX/format.json" 2>"$SANDBOX/format.err"
check "partial report exits 1" test $? -eq 1
jq_check "last format switch controls the partial report" 'length==1' "$SANDBOX/format.json"
jq_check "partial error uses the shared shape" '.code==1' "$SANDBOX/format.err"
fs.erofs "$img" ls / >"$SANDBOX/plain.json" 2>/dev/null
jq_check "plain ls is unchanged" 'length==5 and all(.[]; (has("path") or has("depth") or has("revisited"))|not)' "$SANDBOX/plain.json"
for args in '--max-depth 2' '--limit 2' '-R --limit nope' '-R --max-depth -1'; do
    # shellcheck disable=SC2086 # These words are the command line under test.
    fs.erofs "$img" ls $args / >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
    check "$args exits 2 with no report" test $? -eq 2 -a ! -s "$SANDBOX/usage.out"
    jq_check "usage error uses the shared shape" '.code==2' "$SANDBOX/usage.err"
done
fs.erofs "$img" ls -R /missing >"$SANDBOX/missing.out" 2>"$SANDBOX/missing.err"
check "missing start has no partial report" test $? -eq 1 -a ! -s "$SANDBOX/missing.out"
jq_check "missing start is the not_found kind" '.code==1 and .kind=="not_found"' "$SANDBOX/missing.err"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
fs.erofs --offset 1048576 "$SANDBOX/embedded.img" ls -R / >"$SANDBOX/offset.json" 2>/dev/null
check "offset remains the filesystem offset" cmp -s "$SANDBOX/tree.json" "$SANDBOX/offset.json"
check "image is unchanged" cmp -s "$img" "$SANDBOX/before.img"
finish
