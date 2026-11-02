//! A compressed tail inlined in its inode, read by the real kernel (#266).

mod common;

use common::{dir, open_image_path};
use fs_erofs::mkfs;
use fs_erofs::superblock::EROFS_FEATURE_INCOMPAT_ZTAILPACKING;
use fs_erofs_test_support::{guest_kernel_read, sha256_hex};

/// An inlined tail is a format the kernel has to be told about: Linux 6.1 reads
/// such a file as an ordinary pcluster, and fails to decompress it, unless the
/// superblock sets the ztailpacking feature. `fsck.erofs` accepts it either way,
/// so only the kernel shows this.
#[test]
fn an_inlined_tail_sets_its_feature_and_the_kernel_reads_it() {
    let data: Vec<u8> = (0..20_000u32)
        .flat_map(|i| format!("{} ", i % 300).into_bytes())
        .take(20_000)
        .collect();
    let node = |index_format, ztailpacking| {
        mkfs::Node::CompressedFile(mkfs::CompressedFileSpec {
            mode: mkfs::DEFAULT_FILE_MODE,
            data: data.clone(),
            algo: mkfs::CompressedAlgo::Lz4,
            lclusterbits: 0,
            meta: mkfs::NodeMeta::default(),
            xattrs: Vec::new(),
            index_format,
            ztailpacking,
            target_pcluster_blocks: 1,
        })
    };
    let feature = |img: &[u8]| {
        let (path, _guard) = common::stage_image("tail", img);
        let fs = open_image_path(&path);
        fs.superblock().feature_incompat & EROFS_FEATURE_INCOMPAT_ZTAILPACKING != 0
    };

    let plain = mkfs::build_image(
        dir(vec![(
            "f",
            node(mkfs::CompressedIndexFormat::Legacy, false),
        )]),
        12,
    )
    .unwrap();
    assert!(!feature(&plain), "no tail was inlined");

    let img = mkfs::build_image(
        dir(vec![
            (
                "legacy-tail",
                node(mkfs::CompressedIndexFormat::Legacy, true),
            ),
            (
                "compact-tail",
                node(mkfs::CompressedIndexFormat::Compacted2B, true),
            ),
        ]),
        12,
    )
    .unwrap();
    assert!(feature(&img), "a tail was inlined");

    let (path, _guard) = common::stage_image("tail", &img);
    let out = guest_kernel_read(path.to_str().unwrap(), r#"cd "$MNT" && sha256sum -- *"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let kernel = String::from_utf8(out.stdout).unwrap();
    let want = sha256_hex(&data);
    for name in ["legacy-tail", "compact-tail"] {
        assert!(
            kernel
                .lines()
                .any(|l| l.starts_with(&want) && l.ends_with(name)),
            "{name} did not read back: {kernel}"
        );
    }
}
