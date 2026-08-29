# Human-code report — 2026-08-28

> **This is analysis only. No code was changed.** Phase 0 (Understand) and Phase 1
> (Scan and Triage) were run; Phase 2 (the dev-loop implementation pass) was
> deliberately not started and no branch, commit or source edit was made. The
> working tree contains only this file. Nothing below is fixed — every item is a
> proposal awaiting your decision on scope and order.

**Scope:** the whole crate — `src/*.rs` (14 modules) plus `src/bin/mkfs_erofs.rs`.
Production code only; `#[cfg(test)]` modules were read for coverage assessment but
are not themselves under review except where noted.

**Counts:** 28 items found — **5 High**, **17 Medium**, **6 Low**. 0 fixed, 28 open.

Items M25–M28 and the promotion of M22 came out of a comparison pass against the
six sibling crates, run alongside this review; see *Divergence from the sibling
crates* below. One further divergence — the missing `rust-toolchain.toml` — is not
a readability finding and is called out separately in that section, but it is
probably the most consequential thing in this document.

**Baseline as it stands:** 375 `#[test]` functions (176 unit tests inside `src/`,
199 integration tests across 10 files in `tests/`). `cargo clippy --locked
--all-targets` is clean. `cargo fmt --check` is enforced in CI. The read path is
well covered; the two big files carry 67 unit tests between them.

---

## How this relates to the 2026-08-25 review

`docs/code-quality-review-2026-08-25.md` covered the same `src/` tree three days
ago and found 2 high / 3 medium / 2 low. That review's findings still stand and
are re-stated here where they overlap (H2 ↔ its H1, H4 ↔ its M3, M18 ↔ its M4,
L22 ↔ its L7). This pass differs in three ways:

- It includes `src/bin/mkfs_erofs.rs`, which the earlier pass did not report on.
- It looked for **constant families split across modules**, which surfaced five
  duplications the earlier pass did not (M6–M10).
- The earlier review concluded "**no cross-module duplication at all. One local
  pair, nothing else.**" That is not accurate. This pass found one exact duplicate
  constant definition (M7), a seven-way duplicated test device (M13), and three
  triplicated code shapes (M11, M12, and the mkfs/zmap pack geometry in M6). The
  earlier review's L6 "one duplicated block" is real but is the *least* important
  of them — see H1, which is the same code seen properly.

---

# Findings

Sorted High → Medium → Low. Within a tier, items where test coverage already
exists come first, since those are the ones a dev-loop can move on safely.

---

## H1 — The metadata cursor rule is written twice, 280 lines apart, and the only thing keeping the copies in sync is a comment

- **Files:** `src/mkfs.rs:458-480` (pass 2, address planning) and `src/mkfs.rs:743-757`
  (pass 5, byte emission)
- **Category:** Duplicated code
- **Severity:** High

Both loops walk `bodies` and advance a `meta_cursor` through the metadata area
using the same four rules: round up to a 32-byte slot; sum the four trailer
lengths; compute `block_off`; skip to the next block if the body plus trailers
would straddle one. Pass 2 records the resulting NIDs; pass 5 writes the actual
bytes at the addresses those NIDs imply. If the two ever disagree by a single
byte, every inode in the image lands at an address that does not match its
recorded NID.

The code knows this. `src/mkfs.rs:739-741` says so in prose:

```
// Inodes + their inline trailers. The cursor logic must MIRROR the
// pass-2 layout loop above (including the block-fit skip) or the
// bytes will land at addresses different from the NIDs we recorded.
```

A comment is the wrong enforcement mechanism for an invariant this sharp. There
is one mechanical check — `debug_assert_eq!(meta_cursor / COMPACT_INODE_SIZE, nid)`
at `src/mkfs.rs:757` — and it does not run in the profile that gates pull
requests: `.github/workflows/ci.yml:61` runs `cargo test --locked --release`, and
`[profile.release]` in `Cargo.toml` does not set `debug-assertions`, so the assert
is compiled out. (`release.yml:71` runs the dev profile, so the check does fire —
but only on tag builds, i.e. after the change that would break it has already
merged.)

**Shape of the fix.** One function — `advance_meta_cursor(cursor, body, bs) -> u64`
— called by both loops. The duplication disappears and the invariant becomes
structural rather than advisory. This is the smallest change in this report with
the largest payoff.

**Test coverage:** strong. 40 unit tests in `src/mkfs.rs`, 32 in
`tests/round_trip.rs`, 31 in `tests/oracle_writer.rs`, plus the `fsck.erofs
strict` CI job that validates real emitted images. A dev-loop can extract this
with high confidence.

---

## H2 — `build_image_with` is 473 lines and its stages are named only in comments

- **File:** `src/mkfs.rs:389-861`
- **Category:** God function
- **Severity:** High

The longest function in the crate by a factor of three. It carries its own table
of contents in comment form — `// Pass 1:` (398), `// Pass 2:` (446), `// Pass 3:`
(484), `// Pass 4:` (502), `// Pass 5:` (583) — plus two more unnumbered stages
(COMPR_CFGS encoding at 596, xattr prefix dictionary at 659) and three trailing
emission loops (inodes 742, directory blocks 803, file data 811).

Nine stages, one signature. A reader who wants to check the directory-block
placement has to hold the outputs of four earlier stages in their head to know
what `next_data_block` means when they get there. There is no signature anywhere
saying "given these bodies, produce this block region", so no stage can be read,
tested or changed in isolation.

**Shape of the fix.** The stages already communicate through explicit locals
(`plan`, `bodies`, `nids`, `dir_blocks`, `next_data_block`, the four
`*_for_nid` maps). Each becomes a function returning what the next one consumes.
Do H1 first — the extracted `advance_meta_cursor` is a prerequisite for pass 2
and pass 5 becoming separable.

**Test coverage:** strong, same suites as H1.

---

## H3 — Four module docs claim the crate cannot do things it does

- **Files:** `src/lib.rs:3-6`, `src/fs.rs:3-4`, `src/fs.rs:536-542`, `src/mkfs.rs:3`
- **Category:** Comments that lie
- **Severity:** High

The crate's front door, `src/lib.rs:3-6`, reads:

```rust
//! **Phase 0 scope** — uncompressed images only. Reads superblock,
//! both compact and extended inode shapes, and FLAT_PLAIN /
//! FLAT_INLINE data layouts. Compressed (LZ4 / LZMA / DEFLATE) and
//! chunk-based inodes return `Error::UnsupportedLayout`.
```

None of that is true. `src/decompress.rs` implements all three codecs,
`src/zmap.rs` (2,538 lines) implements the compressed cluster map, `src/chunked.rs`
implements chunk-based inodes, and `src/fs.rs:511-532` dispatches all of them.
`Cargo.toml`'s own description says the crate "reads everything mkfs.erofs 1.9
emits (LZ4/LZMA/DEFLATE, compacted-2B, ztailpacking, fragments, big_pcluster)".

The same false claim repeats at `src/fs.rs:3-4`. `src/mkfs.rs:3` says "Phase 1
(W1) scope: every reader-supported feature **except compressed data layouts**" —
the file contains `PlanKind::Compressed`, `encode_zmap_trailer` and a whole
compression planner. And `src/fs.rs:536-542`, the doc on `read_compressed_block`,
gets two things wrong in one paragraph:

```rust
/// ... decompresses its entire source span at once, and copies the requested
/// block out -- no caching.
/// FRAGMENTS / BIG_PCLUSTER / compacted-1B variants are still flagged
/// via [`zmap::ZMap::open`].
```

The function it documents calls `self.cache_lookup` at line 700 and
`self.cache_insert` at line 744 — it is the crate's caching read path. And
`ZMap::open` no longer flags fragments or big_pcluster; `src/zmap.rs:557-565`
explicitly accepts both.

This is the highest-cost item for a new reader. The first thing they read tells
them the crate is a stub, and the second thing tells them a function does not
cache when it does. Everything they conclude from that point is wrong.

**Test coverage:** none possible — doc comments are not executable. This is the
one item in the report that a test contract cannot protect, which is an argument
for fixing it early rather than late.

---

## H4 — `fill_from_one_pcluster` is 163 lines holding four unrelated strategies

- **File:** `src/fs.rs:584-746`
- **Category:** God function
- **Severity:** High

The longest function on the read path — the one that turns a compressed physical
cluster into file bytes. It contains four independent strategies, each with its
own early return:

1. past-EOF zero-fill (592-599)
2. fragment redirect into the packed inode (610-625)
3. interlaced-PLAIN rotate-and-paste (635-674)
4. non-interlaced PLAIN direct read (675-681)
5. cache lookup, compressed read, decompress, cache insert (699-745)

Mixed in with those are cluster-boundary arithmetic (630-632), device-size
capping (715-724) and codec-config plumbing (738). The arithmetic in particular
would be worth testing on its own, with no decompressor in the loop — right now
it can only be exercised through a real compressed image.

Named in the 2026-08-25 review as M3; restated here as High because it sits
directly beneath H5.

**Test coverage:** good — 24 unit tests in `src/fs.rs`, plus `tests/round_trip.rs`
and `tests/oracle_compat.rs` (37 tests) driving real `mkfs.erofs` output through
this path.

---

## H5 — The pcluster fill loop tests the wrong variable, and the expression shape is what hides it

- **File:** `src/fs.rs:562-572`
- **Category:** Dense, impenetrable expression
- **Severity:** High

```rust
let mut written: usize = 0;
while written < out.len() {
    let cursor = block_start + written as u64;
    self.fill_from_one_pcluster(inode.nid, &zmap, cursor, &mut out[written..])
        .map(|n| written += n)?;
    if written == 0 {
        // Defensive: nothing copied means the resolver advanced
        // past end-of-file. Zero-fill the rest and exit.
        out[written..].fill(0);
        break;
    }
}
```

The guard is meant to catch "this iteration made no progress". It tests
`written == 0`, which is the *cumulative* total. It therefore only fires when the
**first** iteration returns zero. If any later iteration returns zero — after some
bytes have already been copied — `written` is non-zero, the guard does not fire,
`cursor` does not move, and the loop spins forever on the same offset.

The reason this is hard to see is the expression shape. `.map(|n| written += n)?`
uses `map` for a side effect, so the per-iteration return value `n` never appears
as a named local and cannot be compared against zero. Written the ordinary way the
defect is visible in one glance:

```rust
let n = self.fill_from_one_pcluster(...)?;
if n == 0 { out[written..].fill(0); break; }
written += n;
```

Two smaller things fall out of the same block: `out[written..].fill(0)` is reached
only when `written == 0`, so it is `out.fill(0)` written obscurely; and the comment
calls the guard "defensive" while the branch it guards is load-bearing.

Related: `src/fs.rs:631` computes `(extent.source_end_byte - file_offset) as usize`
with no check that `source_end_byte > file_offset`. `pcluster_extent` bounds
`source_end_byte` with `end.min(self.inode.size)` (`src/zmap.rs:1238`), so a
next-head `clusterofs` that lands at or before the requested offset produces a u64
underflow — a panic under `cargo test`, a wrapped huge value under `--release`.
The same restructure makes that reachable to a reader.

**Test coverage:** the surrounding path is well covered, but no test exercises a
zero-return second iteration — which is precisely why the bug is still there. Any
fix here should add one.

---

## M6 — Compacted-pack geometry exists in four encodings, one of which is correct

- **Files:** `src/zmap.rs:442-477` (the good one), `src/zmap.rs:920` and `:936`,
  `src/zmap.rs:602` and `:607`, `src/mkfs.rs:1274-1308`
- **Category:** Magic numbers / duplicated code
- **Severity:** Medium

EROFS's compacted index has two pack shapes: 4-byte packs (`vcnt = 2`,
`pack_bytes = 8`, `encodebits = 16`) and 2-byte packs (`vcnt = 16`,
`pack_bytes = 32`, `encodebits = 14`). `src/zmap.rs` names all six values properly
in `PackGeom::four_byte` / `PackGeom::two_byte`. Then three other places
re-hardcode them.

`locate_compact_pack` holds a `PackGeom` in scope and still writes the numbers by
hand two lines later:

```rust
// src/zmap.rs:920
let initial_bytes = (initial.div_ceil(2)) * 8;
// src/zmap.rs:936
let middle_bytes = (middle.div_ceil(16)) * 32;
```

`ZMap::open` derives the alignment pad from the same family with four unnamed
values:

```rust
// src/zmap.rs:602
let pad = (((32 - (ebase % 32)) / 4) & 7) as u32;
// src/zmap.rs:607
remaining - (remaining % 16)
```

And the writer re-encodes the whole geometry as bare positional arguments at three
call sites:

```rust
// src/mkfs.rs:1277
emit_pack(&entries[entry_cursor..entry_cursor + take], 8, 2, 16);   // 4B
// src/mkfs.rs:1291
emit_pack(&entries[entry_cursor..entry_cursor + take], 32, 16, 14); // 2B
// src/mkfs.rs:1305
emit_pack(&entries[entry_cursor..entry_cursor + take], 8, 2, 16);   // 4B again
```

`emit_pack(slice, 8, 2, 16)` is the least readable line in the crate: four
arguments, three of them unlabelled integers from a domain the reader has to
reconstruct. The abstraction that fixes it already exists 1,300 lines away in the
other file.

**Test coverage:** good on both sides — 27 unit tests in `src/zmap.rs`, 40 in
`src/mkfs.rs`, and `tests/oracle_compat.rs` cross-checks against real
`mkfs.erofs -Ecompacted` output.

---

## M7 — One constant is defined twice with the same value; another is in the wrong module

- **Files:** `src/mkfs.rs:70` and `src/superblock.rs:24`; `src/mkfs.rs:61`
- **Category:** Duplicated code / magic numbers
- **Severity:** Medium

`EROFS_FEATURE_COMPAT_SB_CHKSUM: u32 = 0x0000_0001` is declared in both files,
with the same value and near-identical doc comments. `src/superblock.rs:24` is
`pub` and re-exported from `src/lib.rs:49`; `src/mkfs.rs:70` is a private
redeclaration — in a file that already imports three other constants from
`superblock` two lines earlier (`src/mkfs.rs:40-42`).

`EROFS_FEATURE_INCOMPAT_ZERO_PADDING` (`src/mkfs.rs:61`) has the opposite problem:
it exists only in the writer, but the *reader* depends on the behaviour it names.
`src/decompress.rs:90-98`, `:118-121` and `:205-212` all implement the
zero-padding skip and all three reference the constant by name in prose because
they cannot reference it in code.

The feature-bit family is currently split by direction (read vs write) rather than
by domain. All five feature bits belong together in `superblock.rs`, which already
holds three of them.

**Test coverage:** good — the checksum bit is asserted in `src/superblock.rs`
tests and the padding bit in `tests/round_trip.rs` and `tests/oracle_compat.rs`.

---

## M8 — The block-size validity range is stated three times in two encodings

- **Files:** `src/superblock.rs:110-111`, `src/mkfs.rs:390`,
  `src/bin/mkfs_erofs.rs:115` (plus prose at `:25` and `:72`)
- **Category:** Magic numbers
- **Severity:** Medium

`superblock.rs` does it right and keeps it private:

```rust
const MIN_BLKSZBITS: u8 = 9;
const MAX_BLKSZBITS: u8 = 16;
```

`mkfs.rs:390` writes `if !(9..=16).contains(&blkszbits)`. `bin/mkfs_erofs.rs:115`
writes `if (9..=16).contains(&bits)`. The binary additionally restates the same
constraint twice more in the byte encoding — `"Power of 2, 512..=65536"` in the
usage text at `:25` and `"must be a power of 2 in 512..=65536"` in the error at
`:72` — so the same rule appears five times in three files in two units.

Making the two `superblock` constants `pub` and importing them costs nothing and
collapses all five to one source of truth.

**Test coverage:** all three sites are tested — `src/superblock.rs`
`rejects_bad_blkszbits`, `src/mkfs.rs` `invalid_blkszbits_rejected`, and
`tests/cli.rs` `block_size_below_range_exits_two`.

---

## M9 — Three independent encodings of the same file-type taxonomy

- **Files:** `src/inode.rs:27-34` + `:208-219`, `src/dir.rs:35-44`,
  `src/capi.rs:191-202`
- **Category:** Duplicated code / magic numbers
- **Severity:** Medium

The crate classifies file types three separate ways.

`src/inode.rs` names the POSIX mode bits in hex and maps them to an enum:

```rust
pub const S_IFDIR: u16 = 0x4000;   // ... seven more
pub fn file_type(&self) -> FileType { match self.mode & S_IFMT { S_IFDIR => FileType::Dir, ... } }
```

`src/dir.rs` names the on-disk dirent byte values:

```rust
pub mod ftype { pub const DIR: u8 = 2; pub const REG_FILE: u8 = 1; ... }
```

`src/capi.rs` re-derives the mapping between the two, in octal, ignoring both:

```rust
fn mode_to_abi(mode: u16) -> u8 {
    match mode & 0o170000 {
        0o100000 => 1, // S_IFREG
        0o040000 => 2, // S_IFDIR
        ...
```

The octal literals are the `S_IF*` constants written in a different base, and the
returned integers are exactly `dir::ftype`'s values — but neither is referenced,
so the comments have to re-state the names the code declined to use.
`src/mkfs.rs:35,37` imports *both* `ftype` and the `S_IF*` set and writes exactly
the mapping `capi.rs` reimplements — `src/mkfs.rs:2456-2468` matches on `S_IFCHR`
/ `S_IFBLK` / `S_IFIFO` / `S_IFSOCK` and returns `ftype::CHRDEV` / `ftype::BLKDEV`
/ `ftype::FIFO` / `ftype::SOCK`. The correct version already exists in the crate;
`capi.rs` is the only place that does not use it.

**Test coverage:** good — `src/inode.rs` `predicates_cover_all_file_types` and 29
tests in `tests/capi_basic.rs`.

---

## M10 — Inode sizes 32 and 64 appear as bare literals despite a named constant existing

- **Files:** `src/inode.rs:84,115,119,144,174`, `src/mkfs.rs:346-347`,
  `src/superblock.rs:356`
- **Category:** Magic numbers
- **Severity:** Medium

`src/inode.rs:23` defines `EROFS_INODE_SLOT_SIZE: u64 = 32`, then the parser three
lines below writes the same number five times as a literal: `bytes.len() < 32`,
`on_disk_size: 32`, `bytes.len() < 64`, `on_disk_size: 64`, `[0u8; 64]`.
`src/mkfs.rs:346-347` names them properly but locally:

```rust
const COMPACT_INODE_SIZE: u64 = 32;
const EXTENDED_INODE_SIZE: u64 = 64;
```

So 32 has two names in two files plus five anonymous appearances, and 64 has one
name plus three anonymous appearances. Note that the slot stride and the compact
inode size are *semantically different* quantities that happen to share the value
32 — which is an argument for naming both, not for collapsing them.

Nearby, `src/superblock.rs:356` computes `(sb.sb_extslots as u64) * 16` with the
extension-slot size unnamed, and `src/mkfs.rs:348` builds `SB_AREA_END` from a raw
`128` when `superblock::EROFS_SUPER_BLOCK_SIZE` is public and means exactly that.
The `16` is the 2026-08-25 review's L7.

**Test coverage:** good — `src/inode.rs` has 7 tests including `iloc_math`.

---

## M11 — The codec preamble is written three times in `decompress.rs`

- **File:** `src/decompress.rs:87-103`, `:115-126`, `:202-217`
- **Category:** Duplicated code
- **Severity:** Medium

All three codec wrappers open with the same six lines, differing only in the codec
name inside the error string:

```rust
if input.is_empty() && output.is_empty() { return Ok(()); }
let inputmargin = input.iter().take_while(|&&b| b == 0).count();
let real_input = &input[inputmargin..];
if real_input.is_empty() { return Err(Error::BadInode("LZ4 input is all zeros")); }
```

Three instances is exactly the threshold at which extracting a helper pays. It is
also the one piece of shared *semantics* in the file — the EROFS right-alignment
convention — and each copy carries its own explanatory comment (14 lines at
`:90-98`, 4 at `:118-121`, 8 at `:205-212`), all saying the same thing with
different emphasis. One `strip_leading_pad(input, codec_name)` would hold one
copy of the explanation.

Separately, `decompress` (`:49-55`) and `decompress_with_config` (`:66-81`) are
near-identical three-arm matches; the former could be `decompress_with_config(algo,
None, input, output)`.

**Test coverage:** excellent — 22 unit tests in the file, covering round-trips and
the malformed-input error path for all three codecs.

---

## M12 — The xattr entry header is decoded three times by hand

- **File:** `src/xattr.rs:131-133`, `:174-176`, `:297-298`
- **Category:** Duplicated code
- **Severity:** Medium

`XATTR_ENTRY_HEADER_SIZE` is named at `:44`, but the three-field decode inside it
is open-coded at every use:

```rust
// parse_inline_xattrs
let name_len = buf[cur] as usize;
let name_index = buf[cur + 1];
let value_size = u16::from_le_bytes(buf[cur + 2..cur + 4].try_into().unwrap()) as usize;
// parse_shared_entry — same three lines at offset 0
// read_shared_xattrs — same, minus name_index
```

Two of the three then compute `body_end` with the identical
`checked_add(name_len).and_then(|p| p.checked_add(value_size))` chain. Three
instances, and the third (`read_shared_xattrs`) exists only to learn the entry's
length before re-reading it — which a header struct with a `total_len()` method
would express directly.

**Test coverage:** good — 19 unit tests in the file.

---

## M13 — `MemDev` is defined seven times

- **Files:** `src/chunked.rs:140`, `src/mkfs.rs:2693`, `src/superblock.rs:540`,
  `src/xattr.rs:559`, `src/zmap.rs:1458`, `src/fs.rs:789`, `tests/common/mod.rs:23`
- **Category:** Duplicated code
- **Severity:** Medium

Seven independent in-memory `BlockRead` implementations, in three incompatible
shapes: `MemDev(Mutex<Vec<u8>>)` in four files, `MemDev(Vec<u8>)` in two, and a
`pub`-fielded variant in `chunked.rs`. Their `read_at` bodies are the same
bounds-check-and-copy in every case.

`src/chunked.rs:140` is already `pub(crate)` — the shared crate-internal version
exists and four other test modules do not use it. A single `src/testutil.rs` (or
promoting the `chunked` one) collapses six of the seven; the integration-test copy
in `tests/common/mod.rs` has to stay separate because integration tests link the
crate from outside.

**Test coverage:** these *are* the test infrastructure. Consolidating them is low
risk precisely because every test in the crate exercises one of them.

---

## M14 — `main` in the binary is 79 lines and its exit codes are unnamed

- **File:** `src/bin/mkfs_erofs.rs:30-108`
- **Category:** God function / magic numbers
- **Severity:** Medium

`main` does five things: hand-rolled argument parsing (32-70), block-size
validation (68-74), directory walking (78-85), image building (87-93), and file
writing plus the summary line (95-107). Only the first is long enough to hurt, and
it is the one with the clearest boundary — an `Args { out, src, blkszbits }`
struct and a `parse_args() -> Result<Args, ExitCode>` would leave `main` at about
twenty lines that read as a pipeline.

The exit codes are the second half of this. `ExitCode::from(...)` appears ten
times with three distinct values (`src/bin/mkfs_erofs.rs:39,44,50,57,67,73,84,92,98,107`)
and the convention — 2 for usage errors, 1 for runtime failures, 0 for success —
is never written down. `tests/cli.rs` asserts `Some(2)` in six places and `Some(1)`
in one, so the convention is real and load-bearing; it just has no name.
`EXIT_OK` / `EXIT_FAILURE` / `EXIT_USAGE` next to `USAGE` at `:19` would fix it.

Also here: `walk` (`:124-195`) reaches six levels of indentation inside its
`read_dir` loop, and its two branches build a near-identical `Node::File { mode,
data, meta, xattrs }` (at `:139-146` and `:172-179`).

**Test coverage:** good — 14 tests in `tests/cli.rs`, covering every exit-code arm,
both round-trip paths and the symlink-skip warning.

---

## M15 — `ZMap::map` and `ClusterMapping` are public, unreferenced by any production caller, and documented as something the crate has outgrown

- **Files:** `src/zmap.rs:337-353` and `:1061-1103`, exported at `src/lib.rs:57`
- **Category:** Speculative / superseded code
- **Severity:** Medium

`ZMap::map` is called nowhere in `src/` outside `src/zmap.rs`'s own test module
(11 call sites, all tests). The production read path uses `pcluster_extent`
instead. `ClusterMapping`'s `pcluster_blocks` field documents itself as a
placeholder:

```rust
/// Physical cluster size in BLOCKS. Hard-coded to 1 (one-block
/// pclusters, the LZ4 default) until BIG_PCLUSTER plumbing lands.
pub pcluster_blocks: u32,
```

BIG_PCLUSTER plumbing landed. `src/zmap.rs:1252-1261` decodes CBLKCNT markers and
`PclusterExtent::pcluster_block_count` carries the real count. So a reader
arriving at `ClusterMapping` — which `lib.rs` presents as headline public API,
listed alongside `ZMap` — is told the crate cannot do something it does, in the
same breath as H3.

The two functions also share ~15 lines of NONHEAD walk-back logic verbatim
(`:1074-1093` vs `:1169-1187`), so the dead copy is a live maintenance cost.

**Decision needed, not just a refactor.** Deleting a `pub` item from a published
crate (`am-fs-erofs 0.1.2` is on crates.io) is a breaking change. The options are:
delete in the next minor, keep it and fix the doc, or mark it `#[deprecated]`.
That is your call, which is why this is listed rather than fixed.

**Test coverage:** 11 tests in `src/zmap.rs` cover `map` directly. Nothing outside
the crate is known to use it.

---

## M16 — Three parallel undefined vocabularies for "what is implemented", one of them in a user-facing error

- **Files:** `src/mkfs.rs:3,127,187,211,239,863,1068,1373,1376`,
  `src/fs.rs:536`, `src/decompress.rs:3,10`, plus the "Phase 0" set in H3
- **Category:** Misleading or opaque names
- **Severity:** Medium

The crate labels its own maturity three different ways, none defined anywhere:

- **"Phase 0 / 1 / 2 / 3"** — 18 occurrences across six files. `lib.rs` says
  Phase 0, `decompress.rs:3` says Phase 3, `fs.rs:536` says "Phase 2 v0.3",
  `mkfs.rs:3` says "Phase 1 (W1)".
- **"W1 / W2a / W2b"** — 9 occurrences, all in `mkfs.rs`, used as both scope
  labels and section headers (`// --- compression helpers (W2a) ---` at `:863`).
- **"v0.1 / v0.3"** — `decompress.rs:10`, `fs.rs:536`. Neither matches the actual
  crate version, 0.1.2.

`README.md` does not mention any of them. Neither does any file in `docs/`. They
are internal work-item identifiers from a planning document the reader does not
have.

One of them reaches users. `src/mkfs.rs:1376`:

```rust
return Err(Error::BadInode("lclusterbits > 4 not supported in W2a"));
```

That string is what `mkfs_erofs` prints to stderr and what the C ABI's
`fs_erofs_last_error()` returns. "W2a" means nothing to anyone outside this
repository.

**Test coverage:** n/a for the comments; the error string at `:1376` is reachable
through the public `build_image_with` API.

---

## M17 — Alignment is hand-rolled four times in two idioms and two widths

- **Files:** `src/xattr.rs:159`, `src/xattr.rs:361`, `src/mkfs.rs:711`,
  `src/zmap.rs:586`; plus `src/mkfs.rs:684`
- **Category:** Dense expression / duplicated code
- **Severity:** Medium

Three sites round up to 4 with `(x + 3) & !3` and one rounds up to 8 with
`(header_off + 7) & !7u64`. A fifth site (`src/mkfs.rs:684`) does the same job
with a completely different shape:

```rust
while !xattr_prefix_bytes.len().is_multiple_of(4) { xattr_prefix_bytes.push(0); }
```

The crate targets Rust 2021 on a modern toolchain and already uses `div_ceil` and
`is_multiple_of` elsewhere, so `u64::next_multiple_of(4)` is available and says
what it means. Four instances of the bit-twiddle clears the extraction threshold
on its own; the mixed idioms mean a reader has to recognise three spellings of one
concept.

**Test coverage:** good — all five sites are on paths covered by
`tests/round_trip.rs` and `tests/oracle_writer.rs`.

---

## M18 — 43 lines of production code sit at six levels of indentation or deeper

- **Files:** `src/zmap.rs` (19 lines), `src/mkfs.rs` (15),
  `src/bin/mkfs_erofs.rs` (6), `src/capi.rs` (5), `src/fs.rs` (3)
- **Category:** Deep nesting
- **Severity:** Medium

Measured as production lines starting at column 24 or beyond. Concentrated where a
`match` on index format sits inside a loop over clusters inside a bounds check —
which is the code where a reader most needs to see the shape at a glance.

The binary is the notable new entry: 6 such lines in a 213-line file, all inside
`walk`'s `read_dir` loop. Restated from the 2026-08-25 review's M4, with the
binary added.

**Test coverage:** good in `zmap.rs`/`mkfs.rs`/`cli.rs`.

---

## L19 — `Superblock::u1` is named after its C union, not its meaning

- **File:** `src/superblock.rs:140`, used at `:421` and `src/mkfs.rs:631,636,643,651`
- **Category:** Misleading or opaque name
- **Severity:** Low

The field is either `available_compr_algs` or `lz4_max_distance` depending on a
feature bit, and every use site has to re-explain that. `src/superblock.rs:421`
carries the comment `// available_compr_algs bitmap when COMPR_CFGS is on`;
`src/mkfs.rs` writes into it four times, each with a trailing comment naming the
bit it is setting. Renaming to `compr_algs_or_lz4_distance` (or an accessor pair)
would let those comments go. Note it is `pub` and re-exported, so this is an API
change.

---

## L20 — `is_supported_phase0` is dead outside its own test

- **File:** `src/layout.rs:53-55`
- **Category:** Speculative code
- **Severity:** Low

The only caller is `src/layout.rs:110`, in the test module directly below it. No
production path consults it — `src/fs.rs:494-533` matches on `DataLayout`
exhaustively and supports every variant. It is the H3 lie in executable form.

---

## L21 — A closure takes a parameter and immediately discards it

- **File:** `src/mkfs.rs:1249` and `:1267`
- **Category:** Speculative code
- **Severity:** Low

`emit_pack` takes four parameters, and the third is thrown away:

```rust
let mut emit_pack = |entries_slice: &[CompactEntry], pack_bytes: usize, vcnt: usize, encodebits: usize| {
    ...
    let _ = vcnt;
```

Every call site passes a real value for `vcnt` (2 or 16) which does nothing. Drop
it and the three call sites in M6 get one argument shorter.

---

## M22 — There is no named-offset convention at all: 117 inline hex slice ranges

- **Files:** crate-wide. Representative: `src/inode.rs:84-130`,
  `src/superblock.rs:159-197`, `src/superblock.rs:341-343`, `src/xattr.rs:107`,
  `src/chunked.rs:110`, `src/superblock.rs:356`
- **Category:** Magic numbers
- **Severity:** Medium *(triaged Low on first pass; promoted after the sibling
  comparison — see below)*

Every on-disk field in this crate is read by writing its byte range inline:

```rust
// src/inode.rs:87-91
let raw_format   = u16::from_le_bytes(bytes[0x00..0x02].try_into().unwrap());
let xattr_icount = u16::from_le_bytes(bytes[0x02..0x04].try_into().unwrap());
let mode         = u16::from_le_bytes(bytes[0x04..0x06].try_into().unwrap());
let raw_u        = u32::from_le_bytes(bytes[0x10..0x14].try_into().unwrap());
```

There are **117** such inline ranges across `src/`, and zero named field offsets.
Read in isolation that looks defensible — the ranges are in order, the struct is
documented above, and the first review called this "named offsets are the norm,
with a single exception". Read against the family it is the opposite of the norm.

`rust-fs-btrfs` and `rust-fs-xfs` — the two most recently refactored siblings —
both keep on-disk offsets in documented `pub mod offsets { … }` blocks and use
them at the read sites: 287 and 284 `offsets::` uses respectively, against
erofs's 0. `rust-fs-xfs/src/superblock.rs:44-50` gives the reason from experience:

> two of the three bugs this crate has shipped were exactly that — a value read
> from the wrong place or with the wrong span. A name can be checked against the
> format documentation by eye; `56` cannot.

`rust-fs-btrfs/src/superblock.rs:100-105` says the same: the constants exist "so
that a reader can diff this block against the format documentation directly."

The four originally-flagged lapses are the sharpest instances, because in these
the field name does not even appear in a comment on the same line:
`read_device_table` slices `buf[0..64]`, `buf[64..68]`, `buf[68..72]` for
`tag`/`blocks`/`mapped_blkaddr`; `parse_inline_xattrs` reads `h_shared_count` as
bare `buf[4]`; `compr_cfgs_offset` multiplies by an unnamed `16`;
`lookup_chunk_blkaddr` writes `if info.uses_indexes { 8 } else { 4 }`.

This is the largest single item in the report by line count touched, and it is
also the most mechanical. It does not need to be done all at once — one
`pub mod offsets` per module, starting with `superblock.rs` and `inode.rs`, is a
sequence of independently verifiable commits.

**Test coverage:** excellent throughout — every parser has unit tests with
synthetic buffers, which is exactly the safety net this kind of sweep needs.

---

## L23 — `Error::BadInode` is the crate's catch-all, including for failures that have nothing to do with inodes

- **Files:** `src/decompress.rs` (20 uses), `src/zmap.rs` (24),
  `src/mkfs.rs` (21), `src/fs.rs` (10), `src/superblock.rs` (2)
- **Category:** Misleading name
- **Severity:** Low

77 of the crate's error returns are `BadInode`. Many are not about inodes:
`"LZ4 decompression failed"`, `"DEFLATE decompressed size mismatch"`,
`"COMPR_CFGS walk exceeded sanity bound"`, `"LZMA cfg lc/lp/pb out of range"`,
`"symlink loop"`, `"symlink target not UTF-8"`.

This is visible to consumers. `src/capi.rs:48-56` maps everything that is not
`NotFound`/`NotADirectory`/`OutOfRange`/`NotErofs`/`BadSuperblock` to `EIO`, so a
caller cannot distinguish "the image is corrupt" from "this codec is not
supported" from "you followed a symlink loop". A `BadCompression` and a
`BadSymlink` variant would carry real information to the ABI. Note this changes a
`pub enum`.

---

## L24 — `verify_checksum`'s length calculation is written obscurely

- **File:** `src/superblock.rs:241`
- **Category:** Dense expression
- **Severity:** Low

```rust
let want_len = block_size - off % block_size;
```

`off` is the constant `EROFS_SUPER_OFFSET` (1024). For every block size from 1024
up this is `block_size - 1024`, matching the documented contract at `:220-222`
("length `block_size - EROFS_SUPER_OFFSET`, which is 3072 for the default 4 KiB
block"). For a 512-byte block `off % block_size` is 0 and the expression yields
512, which is not what the doc describes. Whether that is deliberate or accidental
is not decidable from the code — which is the readability problem. Writing the
intent directly, with the 512-byte case handled explicitly, would settle it.

---

## M25 — Two functions take nine and ten parameters, silenced rather than fixed

- **Files:** `src/mkfs.rs:2128-2138` (`write_superblock`, 9),
  `src/mkfs.rs:2185-2196` (`encode_inode`, 10)
- **Category:** Functions with too many parameters
- **Severity:** Medium

```rust
#[allow(clippy::too_many_arguments)]
fn write_superblock(img, blkszbits, n_inodes, total_blocks, meta_blkaddr,
                    feature_incompat, u1, xattr_prefix_count, xattr_prefix_start)

#[allow(clippy::too_many_arguments)]
fn encode_inode(n, idx, nid, body, nids, plan,
                dir_size_for_nid, dir_block_for_nid, data_block_for_nid, _bs)
```

Both allows are bare — no comment explaining why the shape is right. Clippy was
telling the truth here: these are the two functions that consume `build_image_with`'s
accumulated state, and the parameter lists are that state spelled out
positionally. `encode_inode`'s last three arguments are the three `*_for_nid`
maps, which always travel together and are always read the same way — a
`Placement` struct would carry them as one. Its tenth parameter, `_bs`, is unused
and underscore-prefixed, so a caller has to pass a block size that is discarded.

These two are also the natural boundary for H2: they are already the "emit this
region" functions that decomposition would produce, so tightening their
signatures and splitting `build_image_with` are the same piece of work.

By comparison, `rust-fs-ntfs` made the same call once, crate-wide, with a
paragraph of justification at `lib.rs:31-35`, rather than per-site and silent.

**Test coverage:** strong — both are exercised by all 40 `src/mkfs.rs` unit tests
and by the `fsck.erofs strict` CI job.

---

## M26 — `ffi_guard`'s `UnwindSafe` bound does nothing except force 12 call sites to opt out of it

- **File:** `src/capi.rs:90-103`, used at `:263, 293, 338, 357, 376, 413, 443,
  491, 510, 528, 579` (12 `AssertUnwindSafe` occurrences)
- **Category:** Speculative / defensive code for a scenario that cannot happen
- **Severity:** Medium

```rust
fn ffi_guard<T>(fail: T, body: impl FnOnce() -> T + std::panic::UnwindSafe) -> T
```

Every one of the twelve callers wraps its closure in `AssertUnwindSafe(...)` to
satisfy that bound, so the bound never rejects anything — it exists only to be
waived. The cost is that every exported function opens with two lines of ceremony
before the reader reaches what it does:

```rust
ffi_guard(
    std::ptr::null_mut(),
    AssertUnwindSafe(|| {
        clear_last_error();
        ...
```

`rust-fs-xfs/src/capi.rs:120` and `rust-fs-btrfs/src/capi.rs:107` solved this by
applying `AssertUnwindSafe` *inside* the guard, so their call sites read
`guard(-1, || { … })`. Identical panic safety, twelve fewer wrappers, and the
first line of each function is the function's actual job.

erofs inherited the current shape from `rust-fs-squashfs`, its twin; the xfs/btrfs
form is the later refinement.

**Test coverage:** good — 29 tests in `tests/capi_basic.rs`, 20 in
`tests/capi_dir.rs`, 18 in `tests/capi_read.rs`, including panic-path assertions.

---

## M27 — `lib.rs` has no module map, and re-exports 41 identifiers

- **File:** `src/lib.rs:1-20` (missing map), `:40-56` (re-exports)
- **Category:** Misleading structure / opaque entry point
- **Severity:** Medium

Every sibling driver's crate doc ends with a bulleted inventory of its modules —
`rust-fs-squashfs/src/lib.rs:31-38` ("Layout of the reader:"),
`rust-fs-ext4/src/lib.rs:6-15`, `rust-fs-xfs/src/lib.rs:18-27`,
`rust-fs-btrfs/src/lib.rs:41-45`. `src/lib.rs` here has none: the crate doc is a
(false, per H3) scope statement and a `mkfs.erofs` invocation snippet, then
straight into `pub mod` declarations.

This is the crate that needs the map most. It has 15 modules, one of which
(`zmap.rs`) is 2,538 lines of the most intricate code in the family, and nothing
at the entry point tells a reader that `zmap` is the compressed-cluster index,
that `chunked` and `zmap` are alternative data layouts rather than layers, or that
`mkfs` is a writer sharing a crate with a read-only driver.

The re-export surface has the opposite problem — too much rather than too little.
`src/lib.rs:40-56` re-exports 41 identifiers, including raw constants like
`EROFS_XATTR_LONG_PREFIX_MASK`, `EROFS_DIRENT_SIZE` and `EROFS_NULL_ADDR`. Sibling
drivers export 3 to 13: `rust-fs-ext4` and `rust-fs-xfs` export exactly
`{Error, Result, Filesystem, Superblock}`. Only `rust-fs-core`, a framework crate
where a wide prelude is the point, reaches 23.

Every one of those 41 is a semver commitment. Two of them (`ClusterMapping`, and
`EROFS_XATTR_LONG_PREFIX_MASK`) are already in the awkward position described in
M15 — published, and either superseded or an implementation detail.

**Test coverage:** the module map is documentation. The re-export narrowing is a
breaking change and belongs with the M15 decision, not in a refactor pass.

---

## L28 — A 15-line on-disk-spec doc comment sits above a function that only exists in test builds

- **File:** `src/mkfs.rs:1315-1350`
- **Category:** Speculative code / misleading structure
- **Severity:** Low

`plan_compressed_pclusters` carries the crate's fullest prose description of the
greedy pcluster collation policy — the "Option A" algorithm, the PLAIN fallback
rule, the source-byte-range derivation, and a spec citation. Directly beneath it:

```rust
#[cfg(test)]
#[allow(clippy::type_complexity)]
fn plan_compressed_pclusters(
```

The function is compiled only in test builds. The real one,
`plan_compressed_pclusters_with_cfg`, is at `:1362`. So the best explanation of
how the writer decides pcluster boundaries is attached to the copy that never
ships, and a reader who follows the production call graph never reaches it.

Either the doc belongs on the `_with_cfg` function, or — more likely, given the
pattern at `src/decompress.rs:49-55` (M11) — the test-only wrapper should just be
a `_with_cfg(…, None)` call and disappear.

**Test coverage:** it is test-only code; the production path it documents is
covered by `tests/oracle_writer.rs`.

---

# Divergence from the sibling crates

`am-fs-erofs` is one of the two newest members of a seven-crate family
(`rust-fs-core`, `rust-fs-ext4`, `rust-fs-ntfs`, `rust-fs-xfs`, `rust-fs-btrfs`,
`rust-fs-squashfs`). A comparison pass across all seven was run alongside this
review. The headline is that erofs has picked up the family's *test* and
*packaging* conventions almost perfectly, and has not yet picked up its *source
organisation* conventions — which is exactly the profile you would expect from a
recent crate, and it accounts for most of the Medium tier above.

## Not a readability finding, but the most consequential gap: no `rust-toolchain.toml`

**erofs is the only one of the seven crates without a `rust-toolchain.toml`.**

| crate | pinned channel |
|---|---|
| xfs, btrfs, squashfs | 1.94.1 |
| core, ext4, ntfs | 1.95.0 |
| **erofs** | **absent** |

`.github/workflows/ci.yml:25` and `:86` use `dtolnay/rust-toolchain@stable` with
no `toolchain:` input, so this crate builds on floating stable while every sibling
is pinned. Each sibling's toml carries the same warning — floating stable drifts,
a new clippy release adds a lint, and `-D warnings` turns it into a hard CI error
— and `rust-fs-xfs/.github/workflows/ci.yml:39-48` spells out the stronger reason:
these crates are statically linked together into one extension, and a mixed
toolchain across them is a link-time problem, not just a lint problem.

This is outside the human-code remit and no change was made for it, but it is
worth acting on ahead of anything in the findings list.

## Source organisation — where the divergence actually is

**No named-offset convention.** Covered as M22. 117 inline hex slice ranges, zero
`offsets::` uses, against 287 in btrfs and 284 in xfs. This is the single biggest
structural difference between erofs and the two most recently refactored siblings.

**No `constants.rs`, and no sibling has one either** — but the siblings solve the
problem with per-module `pub mod offsets`, and erofs solves it not at all.
Constants here live in the module that first needed them, which is why M6–M10
exist: `EROFS_INODE_SLOT_SIZE` in `inode.rs`, the feature bits split between
`superblock.rs` and `mkfs.rs`, `MIN/MAX_BLKSZBITS` private in `superblock.rs`,
`COMPACT_INODE_SIZE` in `mkfs.rs`, `PackGeom` in `zmap.rs` with its numeric twin
in `mkfs.rs`.

**The writer is one file where every sibling splits it.** `src/mkfs.rs` is 3,918
lines — 32% of the crate — and holds the largest function in the entire family
(H2, 473 lines; next largest anywhere is ext4's `apply_pwrite` at 350). Every
sibling with a write path breaks it into role-named modules:

- xfs: `create.rs`, `dir_write.rs`, `file_write.rs`, `log_write.rs`, `truncate.rs`, `unlink.rs`, …
- btrfs: `extent_write.rs`, `leaf_edit.rs`, `super_write.rs`, `tree_write.rs`, `transaction.rs`, `commit.rs`
- ext4: `extent_mut.rs`, `file_mut.rs`, `htree_mut.rs`, `alloc.rs`, `transaction.rs`, plus a 1,026-line `mkfs.rs`

For proportion: ntfs's writer is 11% of its crate, ext4's is 4%, erofs's is 32%.

Two related notes. **No crate in the family uses cargo features** — there is no
`[features]` section in any of the seven — so erofs is consistent in *not*
gating the writer, and the "put mkfs behind a feature flag" idea from the
2026-08-25 review would be a family-first, not a catch-up. And the writer here is
deliberately not on the C ABI: `include/fs_erofs.h` has no format entry point,
where `rust-fs-ext4` exposes `fs_ext4_mkfs` at `include/fs_ext4.h:611`. That looks
like a real decision, but `src/capi.rs:5` justifies it with "EROFS is an
inherently read-only filesystem", which reads as though no writer exists — worth a
sentence saying "the writer is library- and CLI-only, by choice".

**`src/layout.rs` is named after a sibling concept it does not implement.** In
`rust-fs-xfs`, `src/format/` is on-disk layout constants. Here, `layout.rs` decodes
the `i_format` bitfield into three enums and holds no layout constants at all. A
reader arriving from XFS will look there for M22's missing offsets and not find
them.

**No errno mapping on `Error`.** Two sibling styles exist: `error.rs`-owned
(`rust-fs-squashfs/src/error.rs:12` has `pub mod errno` + `to_errno`, and
`rust-fs-ext4` the same) and `capi.rs`-owned (xfs, btrfs). erofs is in the second
camp, but with the errno constants as bare private file-locals
(`src/capi.rs:41-46`) rather than a named `errno` module — so it matches neither
convention cleanly, and diverges from its own twin, squashfs. This is the
structural half of L23.

**No `endian.rs` and no crate-level byte-order statement.** xfs isolates on-disk
integer decoding in `src/endian.rs` and states the rule once in `lib.rs:7-12`;
btrfs states its own at `lib.rs:7-15`. erofs states "All on-disk fields are
little-endian" only inside `src/superblock.rs:8`, and spells `from_le_bytes` by
hand at all 117 read sites.

**No `lib.rs` module map, 41 re-exports.** Covered as M27.

**`#[allow]` annotation rate.** 13 `#[allow(...)]` in `src/`, 2 of them explained.
For comparison: squashfs 2 (both the standard capi pair), btrfs 4, ntfs 12 of
which 4 explained, xfs 17 of which 12 explained. xfs's habit is worth copying —
`format/log_items.rs:236` reads
`/// Marked #[allow(dead_code)]: a reference module, held to be read.`, repeated
at five more sites. erofs's two explained ones (`src/fs.rs:37-41`,
`src/mkfs.rs:896-899`) are good models already in-house; the other eleven are
bare, including the two `too_many_arguments` in M25 and the
`clippy::missing_safety_doc` at `src/capi.rs:25` (which `rust-fs-ext4/src/capi.rs:42-44`
explains in place, and which xfs and btrfs do not need at all).

## What erofs already does right, and in places better than the family

- **Test layout is family-standard or ahead of it.** `tests/common/mod.rs` (438
  lines, 23 helpers) is the largest in the family; `#[ignore = "reason"]` gating
  matches squashfs and xfs exactly, with 72 gated tests against squashfs's 22 and
  xfs's 13; the `capi_*` / `oracle_*` / `stress` / `large_files` file naming
  matches squashfs one-for-one.
- **`docs/ip-audit-2026-05-09.md` is unique to this crate.**
- **`tests/fixtures/` + `download-gsi.sh` + `tests/oracle_gsi.rs` is unique** — no
  sibling tests against a downloaded real-world production image.
- **`tests/oracle_writer.rs` (984 lines) cross-validating the crate's own writer
  against `fsck.erofs`** has no sibling equivalent, and is what makes H1 and H2
  safe to attempt.
- **`tests/cli.rs` with `assert_cmd`** was an erofs first that squashfs
  subsequently copied.
- **`#![deny(unsafe_op_in_unsafe_fn)]`** puts it with the modern half of the
  family (squashfs, core, xfs, btrfs); ext4 and ntfs, the two oldest, lack it.
- **Cargo.toml packaging is uniform with the family** — edition, crate-type,
  profiles, `am-*` naming, MIT, and per-dependency license/rationale comments on
  the three non-obvious deps.

Two cosmetic gaps in an otherwise-matching packaging story: the `[[bin]]` block at
`Cargo.toml:13-15` is the only one of the family's four without an explanatory
comment (squashfs's explicitly says it "mirrors the mkfs_ext4 / mkfs_erofs binary
convention of the sister crates"), and `panic = "unwind"` at `Cargo.toml:41` is
bare where squashfs, ext4, xfs and btrfs all append
`# required for catch_unwind at the FFI boundary (see src/capi.rs)`.

---

# What to fix first

The order below is chosen so each item makes the next one cheaper, and so the
riskiest extractions happen last, after the code around them has shrunk.

**0. Add a `rust-toolchain.toml` (ten minutes, not a code change).**
Not a finding from this review and not part of any refactor, but it gates
everything else: while CI floats on stable, any of the changes below can go green
today and red tomorrow for reasons unrelated to the change. Pin to whatever the
crates this links against are on — 1.94.1 or 1.95.0 — and move it in lockstep with
them thereafter.

**1. H3 and M16 — the documentation lies (half a day, zero risk).**
Nothing executable changes, `cargo test` cannot regress, and every subsequent
reader of this crate — human or otherwise — stops being misled by the first
paragraph they read. Do this before anything else purely because it is free.
Fold in L20 (delete `is_supported_phase0`) and the `W2a` error string at
`src/mkfs.rs:1376` while you are in there.

**2. H1 — extract `advance_meta_cursor` (one afternoon, well covered).**
The single highest-value change in this report. It is 20 lines moving into a
function, it converts a prose invariant into a structural one, and the test
suite that guards it is the strongest in the crate. It is also a prerequisite for
H2 being tractable.

**3. H5 — rewrite the pcluster fill loop (small, needs a new test).**
Straightening `.map(|n| written += n)?` into a named local exposes the guard
defect. The change itself is four lines; the work is writing the test that pins
the zero-return case, which does not exist today. Do it before H4, because H4
will move this code.

**4. M8, M7, M10 — collapse the constant families (mechanical, well covered).**
Make `MIN/MAX_BLKSZBITS` public and import them in two places; delete the
duplicate `EROFS_FEATURE_COMPAT_SB_CHKSUM` and move
`EROFS_FEATURE_INCOMPAT_ZERO_PADDING` into `superblock.rs`; replace the bare 32s
and 64s. Three small commits, each independently verifiable, and together they
establish where constants live before M6 needs that answer.

**5. M11, M12, M17, M13 — the three-instance duplications (steady, low risk).**
Each is a textbook extraction with the threshold clearly met and good coverage
underneath. M13 (`MemDev` × 7) is the largest line reduction and touches only
test code.

**6. H4 — split `fill_from_one_pcluster` (needs care).**
Four strategies out of one function, and the cluster arithmetic gets its own
tests for the first time. Do it after H5, so you are splitting code you have
already understood.

**6b. M26 and M27's module map — two quick wins, any time.**
Moving `AssertUnwindSafe` inside `ffi_guard` deletes twelve wrappers in one commit
against the crate's best-covered surface (67 capi tests). Writing the `lib.rs`
module map is documentation, costs nothing, and pairs naturally with step 1.

**7. M6, M14 and M25 — pack geometry, the binary, and the wide signatures.**
M6 most needs the constants question settled first; once `PackGeom` (or its
successor) is the single source, both `locate_compact_pack` and `emit_pack`
follow. M25 belongs immediately before H2 — tightening `write_superblock` and
`encode_inode` is the first cut of that decomposition. M14 is independent of
everything else and can be done at any point by anyone.

**8. H2 — decompose `build_image_with` (last).**
473 lines is not a first move. After H1 has extracted the cursor rule, M6/M7 have
named the constants it uses, and M25 has tightened the two functions it feeds, the
pass boundaries are much closer to being functions already, and each extraction
becomes a smaller diff against a suite that has stayed green throughout.

**9. M22 — the named-offset sweep (large, mechanical, do it in slices).**
Deliberately last in the *sequence* despite being Medium, because it touches 117
sites across every module and would collide with anything else in flight. It does
not need to be one change: one `pub mod offsets` per module, `superblock.rs` and
`inode.rs` first, each its own commit. Every parser it touches already has
synthetic-buffer unit tests, so each slice is independently provable.

**Deferred pending your decision, not scheduled:** M15 (`ClusterMapping` is
published API — delete, deprecate or document?), M27's re-export narrowing (same
semver question), L19 and L23 (both change `pub` types), and the larger question
the 2026-08-25 review raised and the sibling comparison sharpens: `src/mkfs.rs` is
32% of this crate against 11% for ntfs and 4% for ext4, it is one file where every
sibling uses six to nine, and it holds two of this report's five High items. Since
no crate in the family uses cargo features, splitting it into modules — the
sibling convention — is the lower-friction half of that question, and could
proceed without settling the feature-gate/separate-crate half at all.
