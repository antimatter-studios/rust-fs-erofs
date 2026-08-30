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

### M6 — compacted-pack geometry in four encodings — **fixed**

EROFS has two pack shapes: 4-byte (`vcnt = 2`, `pack_bytes = 8`,
`encodebits = 16`) and 2-byte (`vcnt = 16`, `pack_bytes = 32`,
`encodebits = 14`). `PackGeom` named all six values; three other places
re-hardcoded them, including one holding a `PackGeom` in scope while writing the
numbers by hand two lines later.

**Probing the coverage first turned up something the report did not have.** The
writer's `emit_pack(slice, 8, 2, 16)` takes `vcnt` as its third argument — and
discards it on the next line but one:

```rust
let _ = vcnt;
```

So that argument was **dead at all three call sites**, and mutating it broke
nothing: `16 → 8` in the 2B call left every test passing. It read as though it
governed how many entries the pack holds; the caller's `take` does that, and
`entries_slice.len()` is the real count. The parameter is gone.

`CompactPackShape` with `COMPACT_4B` / `COMPACT_2B` is the one definition now,
with `bytes_per_lcluster()` and `bytes_for()` for the derived values.
`compact_alignment_pad` replaces the kernel's `((32 - ebase % 32) / 4) & 7`,
whose three numbers are all this geometry wearing no names: 32 is the 2B pack, 4
is the 4B shape's bytes per lcluster, 7 is one less than the number of them that
fit in a 2B pack.

Four tests, **written before the shapes existed and red until they did**. Each
states the format independently rather than restating the code: `encodebits` is
`(pack_bytes - 4) * 8 / vcnt` so a typo in any one field contradicts the other
two; the pad and both region sizes are checked against the literal formulas they
replace, over a full period and beyond so an off-by-one has nowhere to hide.

**Coverage before and after** — mutating each field of the definition:

| field | tests that fail now |
|---|---|
| `COMPACT_2B.encodebits` | 5 |
| `COMPACT_2B.vcnt` | 6 |
| `COMPACT_2B.pack_bytes` | 7 |

The `vcnt` row was **zero** before, in one of the four copies. The refactor moved
a value out of a position where nothing could see it wrong.

### M7, M8, M9, M10, M11, M12, M17, M22 — repeated encodings and unnamed values — **fixable, not yet done**

A constant defined twice and another in the wrong module; the block-size range
stated three times in two encodings; three encodings of the file-type taxonomy;
inode sizes 32 and 64 bare despite a named constant; the codec preamble three
times; the xattr entry header decoded three times by hand; alignment hand-rolled
four times in two idioms and two widths; and **117 inline hex slice ranges with
no named-offset convention at all**.

M22 is the largest and would change how the whole crate reads.

### M13 — `MemDev` defined seven times — **fixed, six of the seven**

Six copies inside `src/` — `chunked`, `mkfs`, `superblock`, `zmap`, `fs`,
`xattr` — plus one in `tests/common/mod.rs`. The bodies differed only in whether
the buffer sat behind a `Mutex` and whether the locals were `start`/`end` or
`s`/`e`.

Test scaffolding duplicates more readily than production code precisely because
nobody is looking for it: each module needed a device, twenty lines was quicker
than finding the one next door, and no reviewer notices a helper only tests use.
The cost is the same as anywhere else — **seven chances for the short-read
behaviour to differ**, and that is the one behaviour a device under test is being
asked about. A device that quietly zero-fills makes a parser reading past its
structure look correct.

`src/test_device.rs` is the one now, with `MemDev::new(bytes)` so callers do not
each spell the `Mutex`. Three tests pin the behaviour the six copies each
restated: a read past the end is a `ShortRead` carrying how much was available
and **leaves the buffer untouched**; an offset past the end reports zero rather
than a wrapped subtraction.

**`tests/common/mod.rs` keeps its copy, deliberately.** Integration tests compile
as a separate crate and cannot see a `#[cfg(test)]` item, so sharing with them
would mean exporting this from the public API behind a feature — changing what
the crate offers the world for the convenience of its own tests. Two copies
across a compilation boundary is a real boundary; seven inside one module tree
was not.

Mutation-checked: making the shared device stop refusing short reads fails 5
tests. 129 lines net removed.

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
