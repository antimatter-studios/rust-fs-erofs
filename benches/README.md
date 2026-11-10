# EROFS LZ4HC performance suite

Run the deterministic workload locally with:

```sh
cargo bench --features bench-reuse,bench-liblz4 --bench erofs_lz4hc
```

To run the same fixed-output path over a directory of real uncompressed
files, pass `--input-dir DIR`. Files are sorted for repeatability. The
benchmark reads them before timing and processes each file in 1 MiB source
windows, matching EROFS's maximum pcluster input. Each encoder writes into
4 KiB, 8 KiB, 16 KiB and 32 KiB destinations at level 12 by default. The
synthetic native-code, mixed and long-run cases cover 64 KiB, 256 KiB and
1 MiB inputs; the original long-run regression input remains a separate case.
Each output row records the corpus byte length and output capacity.
Set `EROFS_LZ4HC_LEVEL` to an integer from 1 through 12 to select another
level; the fork workflow sweeps levels 10, 11 and 12.

The `bench-reuse` feature adds a second Rust measurement for the reusable
workspace API. `lz4hc-workspace` is enabled by default in EROFS builds. Leave
default features and `bench-reuse` off when benchmarking historical fork
commits that predate that API; the stateless Rust and liblz4 paths use the
same benchmark and fixed-output fallback logic either way. CI tests this
stateless compatibility path separately.
if a window does not fit, it continues from the number of input bytes the
encoder reports as consumed. This captures the repeated high-compression work
that whole-buffer `compress_to_vec` benchmarks do not model.

The benchmark times the stateless API, the reusable-workspace API and liblz4
on the same runner. Before timing, both Rust paths and liblz4 must round-trip
the same bytes. It reports the stateless call's transient workspace allocation
and the reusable workspace's retained heap bytes alongside throughput. Each
sample runs for at least 250 ms (and at least three full passes for synthetic
corpora); reported repetitions make the timed input volume visible.
Speedup compares measured MiB/s, since each codec can complete a different
number of corpus passes within that sample window.
The benchmark also fails if Rust stores over 0.4 percentage points more than
liblz4, allowing one additional 4 KiB block for per-file rounding.

The timed loop mirrors `compress_segment`: when the encoded cluster does not
save a 4 KiB block, EROFS stores one plain block and retries from the next
block. Results include attempted and offered input bytes, compressed clusters,
plain fallbacks and padded bytes stored. The incompressible 32 KiB workload
must exercise the fallback path for all three encoders.

The synthetic corpora are deterministic proxies: high-entropy data for already
compressed APK and asset payloads, a structured binary pattern for native
code, source-like text, a mixed corpus, and a long run followed by short runs
that exercises a known HC search-path regression. Before timing, both encoders'
fixed-output blocks are decompressed and compared with the bytes they consumed.
This makes malformed output or incorrect input-consumption accounting fail the
same workflow that records performance. These deterministic proxies support
regressions and fast iteration; use an `--input-dir` corpus before drawing
conclusions about a specific Android partition.

## Real Android image comparison

Run `scripts/bench-real-erofs-lz4hc.sh` to rebuild the pinned
`tests/fixtures/android-system_dlkm.img` tree with the Rust and liblz4
backends. The fixture is the `system_dlkm` partition from Google's Android
16 (API 36) `aosp_atd` arm64 emulator image. Its source URL, archive hash,
partition offset and length, partition hash, and extraction procedure are
pinned and documented in `tests/fixtures/download-android-erofs.sh`; the
image itself is downloaded and is not redistributed in this repository.

The script extracts the source with `fsck.erofs --preserve`, compiles both
backends into separate target directories, and alternates five builds with
the same tree and LZ4HC level 12 / 32 KiB pcluster settings. It runs
`fsck.erofs` on every output and compares extracted manifests for paths,
contents, file types, permissions, ownership, timestamps, hardlink counts,
and xattrs. The benchmark report records the source hash, EROFS and codec
revisions, runner and Rust toolchain, repeated build time and throughput,
peak RSS, and image size.

GitHub runs this in the separate `real EROFS performance` workflow on code
pull requests, nightly, and by manual dispatch. Its summary and artifact keep
the measurements available without making timing variation part of the
required filesystem correctness gate.

The required `EROFS fixed-output LZ4HC performance` job in the `CI` workflow
runs for code changes and merge groups. The lz4_flex fork's `EROFS workload
benchmark` workflow checks out its pull-request revision and runs this suite
against it. Both retain complete output as a GitHub Actions artifact; timing
comparisons stay informational because hosted runner load varies, while the
round-trip checks gate the EROFS CI aggregate.

The C backend is an explicit feature and is absent from the default build.
`lz4_flex` is MIT; the C reference uses `lzzzz` 2.0.0 (MIT) and its bundled
liblz4 (BSD-2-Clause). The EROFS driver has no runtime-selected codec or
dynamic C-library dependency.
