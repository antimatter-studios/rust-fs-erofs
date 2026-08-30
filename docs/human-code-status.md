# Human-code findings — status

Tracks every **High** and **Medium** finding from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md). The report
predates the work; this is the current position. Updated 2026-08-30.

**28 findings** — 5 High, 17 Medium, 6 Low. This covers the 22 High and Medium.

| | High | Medium |
|---|---|---|
| Fixed | 2 | 1 |
| Left for a human decision | 2 | 6 |
| Fixable, not yet done | 1 | 10 |

---

## High

### H1 — the metadata cursor rule is written twice, 280 lines apart — **fixable, not yet done**

Two copies kept in sync by a comment saying they must be. Real, and the fix is
to express the rule once — worth its own change, since getting it wrong
mis-reads every inode.

### H2 — `build_image_with` is 473 lines, its stages named only in comments — **needs your decision**

Accurate. It is also the image builder's entire sequence, where the ordering is
the correctness argument.

### H3 — four module docs claimed the crate cannot do things it does — **fixed**

The crate's front door said:

> **Phase 0 scope** — uncompressed images only. … Compressed (LZ4 / LZMA /
> DEFLATE) and chunk-based inodes return `Error::UnsupportedLayout`.

None of it true. `decompress.rs` implements all three codecs, `zmap.rs` (2,538
lines) implements the compressed cluster map, `chunked.rs` implements chunk-based
inodes, and `fs.rs` dispatches all of them — while `Cargo.toml`'s own
description already said the crate "reads everything mkfs.erofs 1.9 emits".

Corrected in `lib.rs`, `fs.rs` (twice), `mkfs.rs`, `layout.rs` and
`superblock.rs`. The one scope note that *is* still accurate — the **writer**
does not emit compressed data — now says so, and says it is about the writer.

### H4 — `fill_from_one_pcluster` is 163 lines holding four strategies — **needs your decision**

Same category as H2.

### H5 — the pcluster fill loop tested the wrong variable — **fixed earlier**

[#21](https://github.com/antimatter-studios/rust-fs-erofs/pull/21), and its
sequel [#24](https://github.com/antimatter-studios/rust-fs-erofs/pull/24) for
the underflow in the same arithmetic.

---

## Medium

### M16 — three parallel vocabularies for "what is implemented", one in a user-facing error — **fixed**

The user-facing half mattered most:

```
data layout 7 not supported in Phase 0 (compression/chunked)
```

Every data layout is decoded. What actually reaches `UnsupportedLayout` today is
`Algorithm::from_id` meeting an id outside LZ4/LZMA/DEFLATE — so the message
told a user the crate could not do something it does, and named the wrong thing
as the cause.

It now names the id and the valid set. The variant keeps its name because it is
`pub`; its doc explains the mismatch. Its test asserts the new text **and** that
"Phase 0" does not reappear.

### M6, M7, M8, M9, M10, M11, M12, M17, M22 — repeated encodings and unnamed values — **fixable, not yet done**

Compacted-pack geometry in four encodings *one of which is correct*; a constant
defined twice and another in the wrong module; the block-size range stated three
times in two encodings; three encodings of the file-type taxonomy; inode sizes
32 and 64 bare despite a named constant; the codec preamble three times; the
xattr entry header decoded three times by hand; alignment hand-rolled four times
in two idioms and two widths; and **117 inline hex slice ranges with no
named-offset convention at all**.

M6 is the one to do first — four encodings of one geometry, only one right, is a
bug waiting for someone to reach for the wrong one.

M22 is the largest and would change how the whole crate reads.

### M13 — `MemDev` defined seven times — **fixable, not yet done**

### M14, M15, M25, M26, M27 — structure and API surface — **needs your decision**

`main`'s unnamed exit codes; `ZMap::map` and `ClusterMapping` public but
unreferenced and documented as outgrown; two functions taking nine and ten
parameters with the lint silenced rather than the shape fixed; `ffi_guard`'s
`UnwindSafe` bound that does nothing except make 12 call sites opt out; `lib.rs`
re-exporting 41 identifiers with no module map.

M15 and M26 are the two worth deciding soon: both are public surface that exists
only to be worked around.

### M18 — 43 lines at six levels of indentation or deeper — **needs your decision**

---

## Verification

318 tests pass, unchanged in number. `chore lint` clean. Nothing here changes
behaviour; the only visible difference is what an error message says.
