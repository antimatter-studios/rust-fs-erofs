# Test fixtures

This directory holds end-to-end test fixtures that are too large or
license-encumbered to commit into the repo. Everything matching
`*.img`, `*.zip`, or `*.tar*` here is `.gitignore`d.

## `system.img` -- Android GSI

### Purpose

A real Android Generic System Image (GSI), used by `tests/oracle_gsi.rs`
to prove our reader handles a non-toy EROFS image (multi-GB, real
directory layout, real compressed content). The synthetic images we
build with `mkfs::Node` cover the spec; this fixture covers
"production reality."

### Download

```sh
tests/fixtures/download-gsi.sh
```

By default this fetches a SHA256-pinned ARM64 GSI from
`dl.google.com`, verifies the hash, and (if needed) runs `simg2img`
to unwrap an Android-sparse image into a raw EROFS `system.img`.

The pinned URL + hash live at the top of `download-gsi.sh`. To
override (e.g. a newer Android release):

```sh
GSI_URL=https://... GSI_EXPECTED_SHA=... tests/fixtures/download-gsi.sh
```

If the image inside the zip is sparse-wrapped, `simg2img` is required.
On macOS:

```sh
brew install android-platform-tools
```

### Run the test

```sh
cargo test --test oracle_gsi -- --ignored
```

The test is `#[ignore]`-gated so a fresh checkout without the fixture
still has a green `cargo test`.

## License posture

The Android GSI is an Apache-2.0 userspace + GPL-2 kernel image
distributed publicly by Google at
<https://developer.android.com/topic/generic-system-image/releases>.

This repo does NOT redistribute the image -- the download script just
references the public URL. The `.gitignore` ensures the binary never
lands in the tree. We treat the image as an opaque black-box test
input; no AOSP source is copied into this codebase.
