# Code quality review — 2026-08-25

**Scope:** `src/`, 6,275 production lines across 15 files (test modules excluded from
every count below).
**Findings:** 2 high, 3 medium, 2 low. No fixes applied — this is a read of the code
as it stands.

A third of this crate is `mkfs`, which is a much larger proportion than any sibling
(`rust-fs-ntfs` is 10%, `rust-fs-ext4` 5%), and that is where every high-severity
finding sits. The read path — the part that actually ships in a driver — is in good
shape.

---

## H1 — `mkfs::build_image_with` is 473 lines

**`src/mkfs.rs:389`**

The longest single function in the crate by a factor of three, laying out an entire
image: superblock, inode table, directory blocks, compressed clusters and the map
indices that address them.

Image construction is a natural sequence, so the length is not surprising. The problem
is that the sequence is only visible by reading it: there is no signature anywhere
saying "given these inodes, produce this block region", so a reader cannot check one
stage without holding the previous four.

**Shape of the fix.** One function per region produced, each returning the bytes and
the offsets the next stage needs. The stages are already there in the control flow;
they just have no names.

---

## H2 — `zmap.rs` is 1,446 lines and `mkfs.rs` is 1,345

**`src/zmap.rs`, `src/mkfs.rs`**

`zmap.rs` is the compressed-cluster map: the most intricate code in the crate and the
part a reader is most likely to arrive at confused. It holds `pcluster_extent` (156
lines), `open` (102) and `encode_compact2b_index` (75), plus the bulk of the crate's
deep nesting.

The two files together are 44% of the crate. Both name broad concerns, which is what
lets them keep growing.

**Shape of the fix.** For `zmap.rs`, separate the index *decoding* (compact 2-byte,
compact 4-byte, legacy) from the extent *resolution* that consumes it — they are
different problems that currently share a file and, in places, a function.

---

## M3 — 13 functions of 60 lines or more, and one of 163

**`src/fs.rs:584 fill_from_one_pcluster` (163), `src/zmap.rs:1125 pcluster_extent` (156),
`src/zmap.rs:539 open` (102)**

`fill_from_one_pcluster` is the one worth naming separately from the mkfs findings: it
is on the read path, it is the function that turns a compressed physical cluster into
file bytes, and at 163 lines it mixes cluster-boundary arithmetic, decompression
dispatch and output-buffer filling.

Those three are separable and the arithmetic in particular would benefit from being
testable on its own, without a decompressor in the loop.

---

## M4 — 56 lines indented 24 columns or deeper

**mostly `src/zmap.rs`, `src/mkfs.rs`**

Six levels and beyond, concentrated in the map-decoding paths where a `match` on index
format sits inside a loop over clusters inside a bounds check. This is the code where a
reader most needs to see the shape at a glance, and where the indentation most prevents
it.

Early returns for the error cases would flatten a good deal of it without touching the
logic.

---

## M5 — Six `#[allow(...)]`, two of them explained

**`src/superblock.rs` and others**

More suppressions than any sibling, but this crate is also the only one that explains
some of them:

```rust
/// hence `#[allow(dead_code)]`; kept on …
```

That is the right pattern and it should be applied to the other four, including the
`#[allow(clippy::type_complexity)]`. A `type_complexity` suppression is usually a type
alias that has not been named yet, and naming it is generally better than silencing
the lint.

---

## L6 — One duplicated block in `mkfs.rs`

**`src/mkfs.rs:469` and `:747`**

Eight identical lines, twice. The only duplication in the crate, and below the
three-instance threshold at which extracting a helper clearly pays — worth noting so
it does not become a third copy, not worth acting on today.

---

## L7 — One unnamed multi-digit offset

**`src/superblock.rs`**

A single lapse in an otherwise consistently named-offset crate. Mentioned only for
completeness.

---

## What is good, and should survive any refactor

- **The read path is clean.** Excluding `mkfs.rs` and `zmap.rs`, no file exceeds 800
  lines and the module boundaries match the format's own structure.
- **No cross-module duplication at all.** One local pair, nothing else.
- **Named offsets are the norm**, with the single exception in L7.
- **Some `#[allow]`s carry their reasoning**, which is a habit worth spreading to the
  rest of the family.
- **`clippy -D warnings` and `rustfmt` are clean**, and CI enforces both.

## A note on scope

`mkfs` being a third of the crate is worth a decision of its own, separate from any of
the findings above. It is build-time tooling living in the same crate as a read-only
driver, and it is the source of both high-severity findings. Whether it belongs behind
a feature flag, or in its own crate, is a design question rather than a code-quality
one — but it would make this review's H1 and H2 someone else's problem, and shrink what
a driver consumer compiles.

## Suggested order

H1 first: it is the largest single function and its stages are already visible in the
control flow, so extraction is low-risk. M3's `fill_from_one_pcluster` next, since it
is on the read path and the arithmetic inside it deserves its own tests. H2 last, once
the functions are small enough that the file boundaries suggest themselves.
