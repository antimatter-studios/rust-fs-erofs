//! POSIX ACLs, read against an image `mkfs.erofs` built and the kernel
//! mounts.
//!
//! An ACL rides on the xattr path — `system.posix_acl_access`, and on a
//! directory `system.posix_acl_default`, under EROFS name indices 2 and 3
//! — but its value has a format of its own: a version-2 header and one
//! 8-byte `(tag, perm, id)` entry per ACL entry, which `fs_erofs::acl`
//! decodes. Until this suite the only test of either was
//! `tests/round_trip.rs::file_with_acl`, which encodes an ACL with this
//! crate's writer and decodes it with this crate's reader. A misreading
//! shared by both — the wrong name index, the wrong byte order, the wrong
//! tag values — passes that test and disagrees with every real system.
//!
//! So nothing on the expected side here is ours. In the harness guest
//! `setfacl` sets the ACLs, and the kernel applies one more itself, by
//! inheritance from a directory's default ACL; `mkfs.erofs` builds the
//! image from that tree; the kernel mounts it and reports, for every path,
//! the raw ACL values (`getfattr -e hex`) and their meaning (`getfacl
//! -n`). This crate must agree on both, path by path: the bytes, and the
//! entries decoded from them rendered as `getfacl` renders them.
//!
//! The tree covers an ACL with named users, named groups and a mask; one
//! equivalent to a mode, which the kernel stores as no attribute at all;
//! a directory with both an access and a default ACL; a file and a
//! directory that inherited theirs; an ACL of 36 entries; and three files
//! with the same ACL. `-x 1` makes `mkfs.erofs` move any attribute that
//! more than one inode carries into the shared xattr area, and the suite
//! checks that ACLs really were read from both the inline and the shared
//! area, so both decode paths are the ones under test.
//!
//! Out of scope, and why: EROFS is read-only, so there is no ACL this
//! crate writes for the kernel to honour beyond its own `mkfs` writer,
//! which is not what this suite is about.

mod common;

use fs_erofs::acl::{self, AclEntry, AclTag};
use fs_erofs::xattr::{ns, parse_inline_xattrs};
use fs_erofs::{Filesystem, Inode};
use fs_erofs_test_support::{guest_kernel_acl_report, mkfs_from_guest_tree, ScratchDir};
use std::collections::BTreeMap;

const ACCESS: &str = "system.posix_acl_access";
const DEFAULT: &str = "system.posix_acl_default";

/// The tree, staged by the kernel in the guest. Run with an empty source
/// directory as the working directory (see `mkfs_from_guest_tree`).
///
/// The `setfacl -b -k .` comes first: whatever default ACL the guest's
/// scratch directory carries would otherwise be inherited by every path
/// here, and the suite would be testing the guest rather than the image.
const STAGE: &str = r#"umask 022
setfacl -b -k .
printf 'plain\n' > plain.txt
printf 'named\n' > named.txt
setfacl -m u:1001:r--,g:1002:rw-,m::rw- named.txt
printf 'base\n' > mode-only.txt
setfacl -m u::rw-,g::r--,o::--- mode-only.txt
mkdir shared
for i in 1 2 3; do
    printf 'shared %s\n' "$i" > "shared/f$i"
    setfacl -m u:1003:rwx "shared/f$i"
done
mkdir inherit
setfacl -m u:1001:rwx,g:1002:r-x inherit
setfacl -d -m u::rwx,u:1001:r-x,g::r-x,g:1002:rwx,m::rwx,o::--- inherit
printf 'child\n' > inherit/child.txt
mkdir inherit/sub
printf 'many\n' > many.txt
setfacl -m "$(for i in $(seq 2000 2031); do printf 'u:%d:r--,' "$i"; done)m::r--" many.txt"#;

/// Lowercase hex, as `getfattr -e hex` prints a value after its `0x`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `rwx`, `r--` and so on, from the low three permission bits.
fn rwx(bits: u16) -> String {
    let bit = |mask: u16, c: char| if bits & mask != 0 { c } else { '-' };
    format!("{}{}{}", bit(4, 'r'), bit(2, 'w'), bit(1, 'x'))
}

/// One entry as `getfacl -n` prints it.
fn render(entry: &AclEntry, path: &str) -> String {
    let perm = rwx(entry.perm.raw());
    match entry.tag {
        AclTag::UserObj => format!("user::{perm}"),
        AclTag::User => format!("user:{}:{perm}", entry.id),
        AclTag::GroupObj => format!("group::{perm}"),
        AclTag::Group => format!("group:{}:{perm}", entry.id),
        AclTag::Mask => format!("mask::{perm}"),
        AclTag::Other => format!("other::{perm}"),
        AclTag::Unknown(tag) => panic!(
            "{path}: this crate decoded an ACL entry with tag {tag:#x}, which is no POSIX ACL \
             tag; the kernel would have refused the value"
        ),
    }
}

/// What `getfacl -cEn` prints for one path, from what this crate read:
/// the access ACL's entries, or the three a mode implies when there is
/// none, then the default ACL's entries prefixed `default:`.
fn getfacl(path: &str, mode: u16, access: Option<&[u8]>, default: Option<&[u8]>) -> String {
    let decode = |value: &[u8], which: &str| {
        acl::parse(value).unwrap_or_else(|error| panic!("{path}: decoding {which}: {error:?}"))
    };
    let mut lines = match access {
        Some(value) => decode(value, ACCESS)
            .iter()
            .map(|entry| render(entry, path))
            .collect(),
        None => vec![
            format!("user::{}", rwx(mode >> 6)),
            format!("group::{}", rwx(mode >> 3)),
            format!("other::{}", rwx(mode)),
        ],
    };
    if let Some(value) = default {
        lines.extend(
            decode(value, DEFAULT)
                .iter()
                .map(|entry| format!("default:{}", render(entry, path))),
        );
    }
    lines.join(",")
}

/// Every path in the image, as `find -printf '%P'` names it, with its inode.
fn walk(fs: &Filesystem, dir: &Inode, prefix: &str, out: &mut Vec<(String, Inode)>) {
    for entry in fs.read_dir(dir).expect("read_dir") {
        let name = String::from_utf8(entry.name.clone()).expect("utf-8 name");
        if name == "." || name == ".." {
            continue;
        }
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let inode = fs
            .lookup_path(&format!("/{path}"))
            .unwrap_or_else(|error| panic!("lookup /{path}: {error:?}"));
        if inode.is_dir() {
            walk(fs, &inode, &path, out);
        }
        out.push((path, inode));
    }
}

/// The kernel's report, as this crate reads the same image.
fn ours(fs: &Filesystem, paths: &[(String, Inode)]) -> BTreeMap<(String, String), String> {
    let mut report = BTreeMap::new();
    for (path, inode) in paths {
        let xattrs = fs
            .xattrs(inode)
            .unwrap_or_else(|error| panic!("{path}: xattrs: {error:?}"));
        let value = |name: &str| {
            xattrs
                .iter()
                .find(|(n, _)| n.as_slice() == name.as_bytes())
                .map(|(_, v)| v.as_slice())
        };
        let (access, default) = (value(ACCESS), value(DEFAULT));
        let field = |v: Option<&[u8]>| v.map_or_else(|| "-".to_string(), hex);
        report.insert(("access".into(), path.clone()), field(access));
        report.insert(("default".into(), path.clone()), field(default));
        report.insert(
            ("getfacl".into(), path.clone()),
            getfacl(path, inode.mode, access, default),
        );
    }
    report
}

/// How many ACL attributes this image holds inline in their inodes, and
/// how many inodes reach theirs only through the shared xattr area.
///
/// A precondition rather than the subject: it proves the image exercises
/// both places an ACL can live, so agreement above covers both.
fn where_the_acls_live(fs: &Filesystem, image: &[u8], paths: &[(String, Inode)]) -> (usize, usize) {
    let (mut inline, mut shared) = (0, 0);
    for (path, inode) in paths {
        let start = Inode::iloc(fs.superblock(), inode.nid) + u64::from(inode.on_disk_size);
        let end = inode.body_end(fs.superblock());
        let (_, entries) = parse_inline_xattrs(&image[start as usize..end as usize])
            .unwrap_or_else(|error| panic!("{path}: inline xattrs: {error:?}"));
        let is_acl = |index: u8| index == ns::POSIX_ACL_ACCESS || index == ns::POSIX_ACL_DEFAULT;
        let here = entries.iter().filter(|e| is_acl(e.name_index)).count();
        let all = fs
            .xattrs(inode)
            .unwrap_or_else(|error| panic!("{path}: xattrs: {error:?}"))
            .iter()
            .filter(|(n, _)| {
                n.as_slice() == ACCESS.as_bytes() || n.as_slice() == DEFAULT.as_bytes()
            })
            .count();
        inline += here;
        shared += all - here;
    }
    (inline, shared)
}

#[test]
fn acls_the_kernel_set_read_back_as_the_kernel_reads_them() {
    let dir = ScratchDir::new("acl-oracle");
    let image_path = dir.join("acl.img");
    let built = mkfs_from_guest_tree(&image_path, STAGE, &["-b4096", "-x", "1"]);
    assert!(
        built.status.success(),
        "staging the ACL tree and running mkfs.erofs in the guest failed:\n{}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let image_str = image_path.to_str().expect("utf-8 path");
    let kernel = guest_kernel_acl_report(image_str, "acl-oracle");

    // THE KERNEL'S SIDE HAS TO HOLD WHAT THE SUITE IS ABOUT, or agreement
    // means nothing: an image with no ACLs in it agrees with any reader.
    let k = |kind: &str, path: &str| {
        kernel
            .get(&(kind.to_string(), path.to_string()))
            .unwrap_or_else(|| panic!("the kernel reported no {kind} for {path}:\n{kernel:#?}"))
            .as_str()
    };
    for path in [
        "named.txt",
        "inherit",
        "inherit/child.txt",
        "inherit/sub",
        "many.txt",
    ] {
        assert_ne!(k("access", path), "-", "{path} should carry an access ACL");
    }
    for path in ["shared/f1", "shared/f2", "shared/f3"] {
        assert_ne!(k("access", path), "-", "{path} should carry an access ACL");
    }
    for path in ["inherit", "inherit/sub"] {
        assert_ne!(k("default", path), "-", "{path} should carry a default ACL");
    }
    for path in ["plain.txt", "mode-only.txt", "shared"] {
        assert_eq!(k("access", path), "-", "{path} should carry no access ACL");
    }
    assert!(
        k("getfacl", "inherit/child.txt").contains("user:1001:r-x"),
        "inherit/child.txt did not inherit the default ACL: {}",
        k("getfacl", "inherit/child.txt")
    );
    let many = k("getfacl", "many.txt").split(',').count();
    assert_eq!(many, 36, "many.txt: getfacl listed {many} entries, not 36");

    let bytes = std::fs::read(&image_path).expect("read the built image");
    let fs = common::open_image(bytes.clone());
    let root = fs.lookup_path("/").expect("root");
    let mut paths = Vec::new();
    walk(&fs, &root, "", &mut paths);
    let ours = ours(&fs, &paths);

    let (inline, shared) = where_the_acls_live(&fs, &bytes, &paths);
    assert!(
        inline > 0 && shared > 0,
        "the image holds {inline} ACL(s) inline and {shared} in the shared area; both places \
         must be under test (mkfs.erofs -x 1 shares an attribute more than one inode carries)"
    );

    let mut differences = Vec::new();
    for key in kernel.keys().chain(ours.keys()) {
        let (theirs, mine) = (kernel.get(key), ours.get(key));
        if theirs != mine && !differences.iter().any(|(k, _, _)| k == key) {
            differences.push((key.clone(), theirs.cloned(), mine.cloned()));
        }
    }
    assert!(
        differences.is_empty(),
        "this crate and the kernel disagree on {} field(s):\n{}",
        differences.len(),
        differences
            .iter()
            .map(|((kind, path), theirs, mine)| format!(
                "  {path} {kind}\n    kernel: {}\n    ours:   {}",
                theirs.as_deref().unwrap_or("(absent)"),
                mine.as_deref().unwrap_or("(absent)")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!(
        "[acl] {} paths agree with the kernel; {inline} ACL(s) inline, {shared} shared",
        paths.len()
    );
}
