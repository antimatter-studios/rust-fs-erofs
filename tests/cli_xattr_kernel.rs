//! Names and every raw value versus getfattr on mounted reference images.
//! Inline, shared and dictionary entries must all reach the CLI intact.

mod cli_support;

use cli_support::*;
use fs_core::FileDevice;
use fs_erofs::{xattr::parse_inline_xattrs, Filesystem, Inode};
use fs_erofs_test_support::{guest_kernel_read, mkfs_from_guest_tree, ScratchDir};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

const STAGE: &str = r#"
mkdir dir
touch file same bare
ln -s file link
python3 - <<'PY'
import os
for p in [b'file', b'same', b'dir', b'raw-\xff']:
    if p.startswith(b'raw-'):
        open(p, 'wb').close()
    for name, value in [(b'user.comment', b'hello\x00\xff\n'),
                        (b'user.empty', b''), (b'user.dataitem.thing', b'prefix'),
                        (b'user.raw-\xff', b'raw-name'),
                        (b'trusted.audit', b'trusted\x00\xff')]:
        os.setxattr(p, name, value)
    if p != b'dir':
        # Revision 2 capability: effective CAP_NET_BIND_SERVICE.
        os.setxattr(p, b'security.capability', bytes.fromhex(
            '0100000200040000000000000000000000000000'))
os.setxattr(b'file', b'user.unique', b'inline-only')
PY
"#;

const REPORT: &str = r#"
python3 - "$MNT" <<'PY'
import json, os, re, subprocess, sys
for rel in [b'', b'file', b'same', b'bare', b'dir', b'link', b'raw-\xff']:
    p = os.fsencode(sys.argv[1]) + b'/' + rel
    dump = subprocess.run([b'getfattr', b'-h', b'--absolute-names', b'-d',
                           b'-m', b'-', b'-e', b'hex', p], check=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout
    attrs = {}
    for line in dump.splitlines():
        if not line or line.startswith(b'#'):
            continue
        name, value = line.split(b'=', 1)
        name = re.sub(rb'\\([0-7]{3})', lambda m: bytes([int(m[1], 8)]), name)
        assert name not in attrs, dump
        attrs[name] = b'' if value == b'""' else bytes.fromhex(value.removeprefix(b'0x').decode())
    kernel = {os.fsencode(n): os.getxattr(p, n, follow_symlinks=False)
              for n in os.listxattr(p, follow_symlinks=False)}
    assert attrs == kernel, (rel, dump, kernel)
    if rel in [b'file', b'same', b'raw-\xff']:
        assert any(n.startswith(b'trusted.') for n in attrs), rel
        assert any(n.startswith(b'security.') for n in attrs), rel
    entries = []
    for n, v in sorted(attrs.items()):
        entry = {'name': n.decode('utf-8', 'replace')}
        try:
            n.decode('utf-8')
        except UnicodeDecodeError:
            entry['name_hex'] = n.hex()
        entry['size'] = len(v)
        entries.append(entry)
    listing = json.dumps(entries, ensure_ascii=False, indent=2).encode().hex()
    print((b'/' + rel).hex() + '|' + listing + ''.join(
        '|' + n.hex() + '=' + v.hex() for n, v in sorted(attrs.items())))
PY
"#;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn xattr_names_and_values_match_the_kernel_inline_shared_and_with_prefixes() {
    let dir = ScratchDir::new("cli-xattr");
    for dictionary in [false, true] {
        let image = dir.join("xattr.img");
        let mut args = vec!["-b4096", "-x", "1"];
        if dictionary {
            args.push("--xattr-prefix=user.dataitem");
        }
        let built = mkfs_from_guest_tree(&image, STAGE, &args);
        assert!(built.status.success(), "mkfs.erofs: {}", stderr(&built));
        let before = std::fs::read(&image).unwrap();
        let fs = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        let inode = fs.lookup_path("/file").unwrap();
        let start = Inode::iloc(fs.superblock(), inode.nid) + u64::from(inode.on_disk_size);
        let end = inode.body_end(fs.superblock());
        let (shared, inline) = parse_inline_xattrs(&before[start as usize..end as usize]).unwrap();
        assert!(!shared.is_empty(), "fixture must exercise shared xattrs");
        assert!(!inline.is_empty(), "fixture must exercise inline xattrs");
        assert_eq!(!fs.xattr_prefix_dict().unwrap().is_empty(), dictionary);

        let report = guest_kernel_read(image.to_str().unwrap(), REPORT);
        assert!(report.status.success(), "kernel: {}", stderr(&report));
        assert_eq!(stdout(&report).lines().count(), 7);
        for line in stdout(&report).lines() {
            let mut fields = line.split('|');
            let path = unhex(fields.next().unwrap());
            let expected = String::from_utf8(unhex(fields.next().unwrap())).unwrap();
            let attrs: Vec<_> = fields
                .map(|f| {
                    let (n, v) = f.split_once('=').unwrap();
                    (unhex(n), unhex(v))
                })
                .collect();
            if path == b"/file" {
                assert_eq!(attrs.len(), 7, "all namespaces, empty, raw and unique");
            }
            let mut text = Vec::new();
            for (name, value) in &attrs {
                text.push(format!(
                    "{}\t{}",
                    value.len(),
                    String::from_utf8_lossy(name)
                ));
                let mut cmd = tool("fs.erofs");
                cmd.arg(&image)
                    .arg("xattr")
                    .arg(OsStr::from_bytes(&path))
                    .arg(OsStr::from_bytes(name));
                let out = ok(&mut cmd);
                assert_eq!(&out.stdout, value, "{path:?} {name:?}");
                assert!(out.stderr.is_empty());
                let dest = dir.join("value");
                assert!(
                    ok(cmd.args([OsStr::new("--text"), OsStr::new("-o"), dest.as_os_str()]))
                        .stdout
                        .is_empty()
                );
                assert_eq!(std::fs::read(&dest).unwrap(), *value);
                assert!(!dir.join("value.partial").exists());
            }
            let list = ok(tool("fs.erofs")
                .arg(&image)
                .arg("xattr")
                .arg(OsStr::from_bytes(&path)));
            assert_eq!(stdout(&list).trim_end(), expected);
            let out = ok(tool("fs.erofs")
                .arg(&image)
                .args(["xattr", "--text"])
                .arg(OsStr::from_bytes(&path)));
            assert_eq!(stdout(&out).trim_end(), text.join("\n"));
        }
        let embedded = dir.join("embedded");
        let mut bytes = vec![0; 4096];
        bytes.extend_from_slice(&before);
        std::fs::write(&embedded, bytes).unwrap();
        let out = ok(tool("fs.erofs").arg(&embedded).args([
            "--offset",
            "4096",
            "xattr",
            "/file",
            "user.comment",
        ]));
        assert_eq!(out.stdout, b"hello\0\xff\n");
        assert_eq!(std::fs::read(&image).unwrap(), before);
    }
}
