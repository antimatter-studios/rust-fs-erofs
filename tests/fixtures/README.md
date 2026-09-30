# Test fixtures

This directory holds end-to-end test fixtures that are not ours to
commit. Everything matching `*.img`, `*.zip`, or `*.tar*` here is
`.gitignore`d.

## `android-system_dlkm.img` -- a real Android EROFS image

### Purpose

An EROFS image that neither this repository nor `mkfs.erofs` in a temp
directory produced: the `system_dlkm` partition of Google's Android 16
(API 36) `aosp_atd` arm64 emulator system image, as Google's build wrote
it. 7,397,376 bytes, 108 inodes, 98 of them LZ4-compressed, big
pclusters, `0padding`, and a `security.selinux` label on every inode.
`tests/oracle_android.rs` reads every inode and compares it against
`android-system_dlkm.manifest`, which is committed.

### Where the expectation comes from

`android-system_dlkm.manifest` is what two readers that are not this
crate saw in the same bytes: the Linux 6.1 kernel's EROFS driver, the
image loop-mounted read-only, and `fsck.erofs --extract`, which agreed
on every path, type, size and content hash. So a failure is this crate
misreading a real image, not two of our readers disagreeing.

### Download

```sh
tests/fixtures/download-android-erofs.sh
```

Downloads the 712 MB emulator zip from `dl.google.com`, checks its SHA-1
against Google's own repository manifest, cuts the partition out of
`arm64-v8a/system.img` at a pinned offset, and refuses the result unless
it carries the EROFS magic -- naming the filesystem it found instead --
and matches a pinned SHA-256. Needs `curl`, `unzip` and `sha1sum` /
`sha256sum` (or `shasum`). `ANDROID_EROFS_ZIP=<path>` uses a zip you
already have.

### Why not a GSI

This used to fetch an Android Generic System Image. Every GSI on
developer.android.com -- Android 16 and 17, aosp and gms -- ships an
**ext4** `system.img`, checked at each one's superblock on 2026-09-30,
so that suite could never pass (#133). The emulator image's
`system_dlkm` is the one EROFS partition in either image family.

### Run the test

```sh
chore test:android
```

`test:android` is its own tier: `chore test` does not run it, because a
pull request cannot be made to depend on a 712 MB download. The nightly
`.github/workflows/android.yml` does, with the fixture cached on its
SHA-256. Inside that tier a missing fixture **fails**, naming the
script. The suite it replaced used to be `#[ignore]`-gated and to print
"skipping" and return, which is how it reported ok on every run without
ever opening an image (#54).

## License posture

The emulator image is distributed by Google under the Android SDK
license, and `system_dlkm` holds kernel modules. This repository does
NOT redistribute it: the download script references the public URL, and
`.gitignore` keeps the binary out of the tree. The manifest -- paths,
sizes, hashes and labels -- is ours.
