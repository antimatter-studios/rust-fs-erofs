//! Recursive metadata versus GNU find on a mounted reference-made image.
mod cli_support;

use cli_support::*;
use fs_erofs_test_support::{guest_kernel_read, mkfs_from_guest_tree, ScratchDir};
use std::collections::BTreeMap;

#[test]
fn recursive_listing_matches_the_kernel_walk() {
    let dir = ScratchDir::new("recursive-kernel");
    let image = dir.join("tree.img");
    let built = mkfs_from_guest_tree(
        &image,
        r#"
mkdir -p a/deep empty z
printf payload > a/deep/file
printf space > 'two words'
ln a/deep/file hard
ln -s a dirlink
ln -s ../.. a/deep/up
ln -s absent dangling
printf raw > $'a/deep/raw-\xff'
printf raw > $'a/deep/raw-\xfe'
"#,
        &["-b4096"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let image_arg = image.display().to_string();
    let before = std::fs::read(&image).unwrap();
    let oracle = guest_kernel_read(
        &image_arg,
        "find \"$MNT\" -mindepth 1 -printf '%P\\0%y\\0%s\\0%i\\0'",
    );
    assert!(oracle.status.success(), "{}", stderr(&oracle));
    let mut expected = BTreeMap::new();
    let fields: Vec<_> = oracle.stdout.split(|b| *b == 0).collect();
    assert_eq!(fields.last(), Some(&b"".as_slice()));
    assert_eq!((fields.len() - 1) % 4, 0);
    for row in fields[..fields.len() - 1].chunks_exact(4) {
        let mut path = b"/".to_vec();
        path.extend_from_slice(row[0]);
        let number = |bytes: &[u8]| std::str::from_utf8(bytes).unwrap().parse::<u64>().unwrap();
        let kind = match row[1] {
            b"d" => "dir",
            b"f" => "file",
            b"l" => "symlink",
            other => panic!("unexpected find type {other:?}"),
        };
        assert!(expected
            .insert(path, (kind.to_string(), number(row[2]), number(row[3])))
            .is_none());
    }
    assert_eq!(expected.len(), 12, "the oracle must walk the whole tree");
    let output = stdout(&ok(tool("fs.erofs").args([&image_arg, "ls", "-R", "/"])));
    let mut actual = BTreeMap::new();
    let mut order = Vec::new();
    for row in output.split("\n  {").skip(1) {
        let path = if row.contains("\"path_hex\"") {
            let hex = json_field(row, "path_hex");
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect::<Vec<_>>()
        } else {
            json_field(row, "path").into_bytes()
        };
        assert_eq!(
            json_field(row, "depth").parse::<usize>().unwrap(),
            path.iter().filter(|b| **b == b'/').count()
        );
        assert!(!row.contains("\"revisited\""));
        order.push(path.clone());
        let value = (
            json_field(row, "type"),
            json_field(row, "size").parse::<u64>().unwrap(),
            json_field(row, "inode").parse::<u64>().unwrap(),
        );
        assert!(actual.insert(path, value).is_none(), "duplicate path");
    }
    assert_eq!(actual, expected);
    assert_eq!(order, expected.keys().cloned().collect::<Vec<_>>());
    assert_eq!(std::fs::read(&image).unwrap(), before);
}
