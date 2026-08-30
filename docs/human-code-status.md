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

### H1 — the metadata cursor rule is written twice, 280 lines apart — **fixed**

This entry was stale: it was fixed and never re-marked. `plan_meta_layout`
returns `MetaSlot`s and is the single walk of the metadata area; pass 2 reads the
NIDs out of them, and pass 5 zips over the same slots rather than re-deriving
addresses with a second cursor. The comment that used to say the two loops "must
MIRROR" each other is gone, because there is no second loop to mirror.

The invariant is now structural: an inode's bytes land at the address its
recorded NID names because both come out of the same slot, not because two
walks agree.

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

### M12 — the xattr entry header decoded three times by hand — **fixed**

`XATTR_ENTRY_HEADER_SIZE` was named; the decode inside it was not, and was
open-coded at three sites.

**The third was the odd one out twice over.** `read_shared_xattrs` reads a header
only to learn how far to read again, and it dropped `name_index` entirely and
summed the body length with a plain `+` where the other two used `checked_add`.
Neither is reachable — a `u8` and a `u16` cannot overflow a 64-bit `usize` — but
a third copy that validates less than its siblings is how the reachable version
eventually arrives. That is the same shape as the NTFS `".."` bug, where the
fifth copy of a basename check was the weak one.

`XattrEntryHeader` with `parse`, `total_len` and `is_padding` is the one
definition now. `total_len` is checked, so the arithmetic is the same at every
site rather than at two of three.

**All three sites were covered before, which is what made this safe rather than
merely tidy** — flipping the byte order at each in turn failed 5, 3 and 3 tests.
Four new tests pin the shape once, and mutating the single definition now fails:

| mutation | tests failing |
|---|---|
| byte order | 8 |
| `name_index` offset | 7 |
| padding predicate widened | 2 |

The byte-order test asserts against literal bytes rather than `from_le_bytes`,
because a slip there is invisible to a round trip through this crate: writer and
reader would agree while disagreeing with `mkfs.erofs`.

### M7, M8, M9, M10, M11, M17 — repeated encodings and unnamed values — **fixed**

- **M7** — both feature bits now live in `superblock.rs`, which already held
  three others. `EROFS_FEATURE_INCOMPAT_ZERO_PADDING` moving there matters more
  than the tidiness: the *reader* implements the behaviour it names, at three
  sites in `decompress.rs`, and each of those could previously only refer to the
  bit in prose because it was private to the writer.
- **M8** — `MIN_BLKSZBITS` / `MAX_BLKSZBITS` are `pub`, with `MIN_BLOCK_SIZE` /
  `MAX_BLOCK_SIZE` derived from them and `is_valid_blkszbits`. The CLI's usage
  text and error message are now *generated* from those, so the five copies in
  three files and two units are one definition. The binary can no longer
  advertise a range it does not enforce.
- **M9** — one `dir::dirent_type_for_mode`. `capi.rs`'s third table — the one
  written in octal, whose comments named the `S_IF*` constants and whose return
  values were the `ftype` constants, while referencing neither — is now an import.
  The writer's two device/special arms go through the same function and filter
  its result, which is where the `PlanKind` genuinely adds something.
- **M10** — `COMPACT_INODE_SIZE`, `EXTENDED_INODE_SIZE` and `SB_EXTSLOT_SIZE`
  live in `inode.rs` and the writer imports them. `EROFS_INODE_SLOT_SIZE` stays
  separate with a doc saying why: it shares the value 32 with the compact inode
  size and is a *different quantity* — an addressing stride against a structure
  size — and collapsing them would lose exactly the distinction that makes the
  extended case legible.
- **M11** — one `strip_leading_pad(input, codec)`, holding one copy of the
  explanation that was written three times at 14, 4 and 8 lines. The
  consolidated doc is longer than any of them, because it records the *separate*
  reason each codec's first byte is never zero — LZ4's token, DEFLATE's
  BFINAL/BTYPE bits, LZMA's packed properties byte. The assumption is per-codec,
  and the old comments each stated only their own.
- **M17** — `align_to_xattr_entry` / `XATTR_ENTRY_ALIGN` and
  `COMPACT_MAP_EBASE_ALIGN`, via `next_multiple_of`. Three spellings of one rule
  became one, and the bit-twiddle's silent wrap near `u64::MAX` became a panic —
  neither is reachable from a real image, but only one of them says so.

### M22 — 117 inline hex slice ranges with no named-offset convention — **the two structural layouts done, the rest deliberately not**

`superblock::offsets`, `inode::compact_offsets` and `inode::extended_offsets`
name every field of the two structures whose layouts are fixed by the kernel
header, and **both the parser and the writer index them** — so a field can no
longer move on one side without moving on the other. That is 52 of the ranges in
the two parsers plus 27 in the writer.

The compact and extended inodes are two tables, not one with exceptions: they
genuinely diverge after 0x08 (`SIZE` is four bytes in one and eight in the
other, `UID` two and four), and encoding that in a single table would be harder
to check against the header than two plain lists. A test asserts the five fields
read *before* the version is known sit at the same offsets in both — the parser
reads `format` to find out which layout it has, so if those diverged it could
not work at all.

The remaining ranges — 6 in `fs.rs`, 4 in `zmap.rs`, 1 each in `xattr.rs` and
`chunked.rs` — are left. They are not one structure's layout; they are local
byte manipulation inside routines that already name what they are doing, and a
table for them would be a table of one entry each.

**The test fixtures keep their literal offsets**, as in rust-img-vhd and
rust-img-qcow2, and for the same measured reason: moving the superblock's
`META_BLKADDR` by a byte fails 44 tests, the compact inode's `MODE` 145, the
extended inode's `MTIME` 3 — all because the fixtures were written from the
kernel header and do not import the tables. `superblock_offsets_match_the_kernel_header`,
`inode_offsets_match_the_kernel_header` and the two overlap tests write that
intent down where a later tidy-up will meet it.

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
