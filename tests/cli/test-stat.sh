# The installed stat contract on both inode layouts. These small byte
# fixtures exercise presentation; cli_stat_kernel.rs independently checks
# every stored field on mounted images made by reference mkfs.erofs.
source "$(dirname "$0")/lib.sh"

for layout in compact extended; do
    img="$SANDBOX/$layout.img"
    python3 - "$img" "$layout" <<'PY'
import struct, sys
b = bytearray(5 * 4096)
def put(off, fmt, *values):
    struct.pack_into('<' + fmt, b, off, *values)
put(1024, 'I', 0xe0f5e1e2)
put(1036, 'BBHQQIII', 12, 0, 1, 5, 1700000000, 123456789, 5, 1)
extended = sys.argv[2] == 'extended'
entries = [(b'.', 1, 2), (b'..', 1, 2), (b'd', 3, 2),
           (b'dev', 9, 3), (b'f', 5, 1), (b'link', 7, 7)]
off = len(entries) * 12
for i, (name, nid, kind) in enumerate(entries):
    put(8192 + i * 12, 'QHBB', nid, off, kind, 0)
    b[8192 + off:8192 + off + len(name)] = name
    off += len(name)
for nid, mode, size, raw, links in [(1, 0o40755, off, 2, 3),
        (3, 0o40755, 0, 0, 2), (5, 0o100640, 5, 3, 1),
        (7, 0o120777, 1, 4, 1), (9, 0o20600, 0, 0x123abc45, 1)]:
    at = 4096 + nid * 32
    put(at, 'HHH', int(extended) | 0xabc0, 0, mode)
    put(at + 16, 'II', raw, nid)
    if extended:
        put(at + 8, 'Q', size)
        put(at + 24, 'IIQII', 1234, 5678, 1000000000, 987654321, links)
    else:
        put(at + 6, 'HI', links, size)
        put(at + 24, 'HH', 1234, 5678)
b[12288:12293] = b'hello'
b[16384] = ord('f')
with open(sys.argv[1], 'wb') as out:
    out.write(b)
PY
    check "$layout fixture is built" test $? -eq 0
    cp "$img" "$SANDBOX/before.img"
    shared='["path","inode","type","mode","uid","gid","nlink","size","allocated_bytes","atime","atime_nsec","mtime","mtime_nsec","ctime","ctime_nsec","btime","btime_nsec"]'
    for name in / /d /f /link /dev; do
        fs.erofs "$img" stat "$name" >"$SANDBOX/stat.json" 2>"$SANDBOX/stat.err"
        rc=$?
        check "$layout stat $name exits 0 ($(cat "$SANDBOX/stat.err"))" test "$rc" -eq 0
        jq_check "shared keys in order" "keys_unsorted[0:17] == $shared" "$SANDBOX/stat.json"
        jq_check "every key is typed; absent times are null" \
            '(.path|type)=="string" and (.inode|type)=="number" and (.type|type)=="string" and (.mode|test("^[0-7]{4}$")) and all(.uid,.gid,.nlink,.size,.allocated_bytes,.mtime,.mtime_nsec; type=="number") and all(.atime,.atime_nsec,.ctime,.ctime_nsec,.btime,.btime_nsec; .==null) and .erofs.xattr_count==0 and .erofs.flags=="0xabc0"' "$SANDBOX/stat.json"
        jq_check "inode layout is $layout" ".erofs.layout == \"$layout\" and .erofs.datalayout == \"flat_plain\"" "$SANDBOX/stat.json"
    done
    fs.erofs "$img" stat /link >"$SANDBOX/link.json" 2>/dev/null
    jq_check "symlink is not followed" '.type=="symlink" and .target=="f" and .size==1' "$SANDBOX/link.json"
    fs.erofs "$img" stat /dev >"$SANDBOX/dev.json" 2>/dev/null
    jq_check "device numbers" '.type=="char" and .major==2748 and .minor==74565' "$SANDBOX/dev.json"
    fs.erofs "$img" stat /f >"$SANDBOX/f.json" 2>/dev/null
    jq_check "file metadata" '.type=="file" and .mode=="0640" and .uid==1234 and .gid==5678 and .nlink==1 and .size==5 and .allocated_bytes==4096' "$SANDBOX/f.json"
    if [ "$layout" = compact ]; then seconds=1700000000; nanos=123456789; else seconds=1000000000; nanos=987654321; fi
    jq_check "effective mtime" ".mtime==$seconds and .mtime_nsec==$nanos" "$SANDBOX/f.json"
    fs.erofs "$img" stat --text /f >"$SANDBOX/f.txt" 2>/dev/null
    check "text exits 0" test $? -eq 0
    check "text key: value" grep -qx 'size: 5' "$SANDBOX/f.txt"
    fs.erofs "$img" stat /missing >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "missing path exits 1" test $? -eq 1
    check "missing path stdout empty" test ! -s "$SANDBOX/out"
    jq_check "missing path structured error" '.code==1 and (.error|test("not found"))' "$SANDBOX/err"
    fs.erofs "$img" stat >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "missing argument exits 2" test $? -eq 2
    check "usage stdout empty" test ! -s "$SANDBOX/out"
    jq_check "usage structured error" '.code==2' "$SANDBOX/err"
    { head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
    fs.erofs --offset 1048576 "$SANDBOX/embedded.img" stat /f >"$SANDBOX/off.json" 2>/dev/null
    check "offset is filesystem offset" cmp -s "$SANDBOX/f.json" "$SANDBOX/off.json"
    check "image unchanged" cmp -s "$img" "$SANDBOX/before.img"
done
finish
