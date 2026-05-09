//! EROFS extended-attribute (xattr) parsing.
//!
//! Inline xattrs sit immediately after the inode body (compact 32 /
//! extended 64 bytes). Their total inline area in bytes is
//! `12 + (xattr_icount - 1) * 4` when `xattr_icount > 0`, else 0.
//! The 12 is the size of `struct erofs_xattr_ibody_header`.
//!
//! Layout (after the 12-byte header):
//!
//! ```text
//! [shared_idx0 u32][shared_idx1 u32]...[shared_idx(h_shared_count-1) u32]
//! [entry0][pad to 4][entry1][pad to 4]...[entryK-1][pad to 4]
//! ```
//!
//! The `h_shared_count` u32 indices come FIRST (right after the header),
//! followed by the inline `erofs_xattr_entry` records. Each shared index
//! `i` resolves to a stand-alone entry at byte offset
//! `sb.xattr_blkaddr * block_size + i * 4` -- the same wire format as an
//! inline entry, just stored in a dedicated dedup area.
//!
//! Entry header (4 bytes) followed by name + value:
//!
//! - 0x00 `e_name_len` (u8)
//! - 0x01 `e_name_index` (u8) -- namespace, see `resolve_full_name`. When
//!   the high bit (`0x80`) is set, the low 7 bits index the custom-prefix
//!   dictionary at `sb.xattr_prefix_start * 4` (NOT `* block_size` -- the
//!   field is a 4-byte-aligned byte offset divided by 4).
//! - 0x02 `e_value_size` (u16 LE)
//!
//! Sources: EROFS on-disk format documentation
//! (<https://erofs.docs.kernel.org/en/latest/design.html#extended-attributes>),
//! the `erofs_xattr_*` and `EROFS_XATTR_INDEX_*` definitions in the
//! public format header `erofs_fs.h`, and the POSIX xattr namespace
//! conventions in the uapi `xattr.h` header. Verified empirically
//! against `mkfs.erofs` (erofs-utils 1.9) output. Independent
//! implementation -- no kernel/erofs-utils sources consulted.

use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::superblock::Superblock;
use fs_core::BlockRead;

pub const XATTR_HEADER_SIZE: usize = 12;
const XATTR_ENTRY_HEADER_SIZE: usize = 4;

/// Bit set in `e_name_index` to flag a custom-prefix-dictionary entry.
/// When set, the low 7 bits (`name_index & EROFS_XATTR_LONG_PREFIX_MASK`)
/// are an index into the dictionary at `sb.xattr_prefix_start * 4`.
///
/// Spec: `linux/fs/erofs/xattr.h::EROFS_XATTR_LONG_PREFIX`. Independent
/// implementation, value confirmed by inspecting mkfs.erofs output.
pub const EROFS_XATTR_LONG_PREFIX: u8 = 0x80;
pub const EROFS_XATTR_LONG_PREFIX_MASK: u8 = 0x7F;

/// Namespace index byte values stored in `e_name_index`.
pub mod ns {
    pub const RAW: u8 = 0;
    pub const USER: u8 = 1;
    pub const POSIX_ACL_ACCESS: u8 = 2;
    pub const POSIX_ACL_DEFAULT: u8 = 3;
    pub const TRUSTED: u8 = 4;
    pub const LUSTRE: u8 = 5;
    pub const SECURITY: u8 = 6;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrEntry {
    pub name_index: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// One entry in the custom-prefix dictionary. `base_index` selects the
/// underlying namespace whose canonical prefix is prepended; `infix` is
/// a constant string sandwiched between that prefix and the per-entry
/// `name` bytes. So a dict entry `{ base_index = USER, infix = "dataitem" }`
/// at dict index 0 turns an inline entry with `name_index = 0x80, name = ".thing"`
/// into the full attribute name `user.dataitem.thing`.
///
/// Spec: `linux/fs/erofs/xattr.h::erofs_xattr_long_prefix`. Independent
/// implementation -- format verified against mkfs.erofs output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrLongPrefix {
    pub base_index: u8,
    pub infix: Vec<u8>,
}

/// Parse the inline xattr area. Returns `(shared_indices, inline_entries)`.
///
/// `buf` must be exactly the inline xattr region: `12 + (icount - 1) * 4`
/// bytes when present. An empty buffer yields `(vec![], vec![])` -- it
/// represents the `xattr_icount == 0` case.
///
/// Note the layout: the `h_shared_count` u32 indices come FIRST (right
/// after the 12-byte header), THEN the inline entries. This is the
/// opposite order from what an early reading of the kernel doc might
/// suggest -- the on-disk reality is shared-indices-first, confirmed by
/// inspecting `mkfs.erofs` (erofs-utils 1.9) output byte-for-byte.
pub fn parse_inline_xattrs(buf: &[u8]) -> Result<(Vec<u32>, Vec<XattrEntry>)> {
    if buf.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if buf.len() < XATTR_HEADER_SIZE {
        return Err(Error::BadXattr("buffer shorter than xattr header"));
    }

    let shared_count = buf[4] as usize;
    let shared_bytes = shared_count * 4;
    let shared_end = XATTR_HEADER_SIZE
        .checked_add(shared_bytes)
        .ok_or(Error::BadXattr("shared count overflow"))?;
    if shared_end > buf.len() {
        return Err(Error::BadXattr("shared indices extend past inline area"));
    }

    let mut shared = Vec::with_capacity(shared_count);
    for i in 0..shared_count {
        let off = XATTR_HEADER_SIZE + i * 4;
        shared.push(u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()));
    }

    let mut out = Vec::new();
    let mut cur = shared_end;
    let entries_end = buf.len();
    while cur < entries_end {
        // Trailing padding within the inline area is allowed and may be
        // all zeros. A sub-4-byte gap at the end is benign.
        if entries_end - cur < XATTR_ENTRY_HEADER_SIZE {
            break;
        }
        let name_len = buf[cur] as usize;
        let name_index = buf[cur + 1];
        let value_size = u16::from_le_bytes(buf[cur + 2..cur + 4].try_into().unwrap()) as usize;

        if name_len == 0 && value_size == 0 && name_index == 0 {
            // Treat as terminal padding -- no real entry has zero in all
            // three fields (ACL slots have value_size > 0).
            break;
        }

        let entry_body_start = cur + XATTR_ENTRY_HEADER_SIZE;
        let entry_body_end = entry_body_start
            .checked_add(name_len)
            .and_then(|p| p.checked_add(value_size))
            .ok_or(Error::BadXattr("entry size overflow"))?;
        if entry_body_end > entries_end {
            return Err(Error::BadXattr("entry runs past inline area"));
        }

        let name = buf[entry_body_start..entry_body_start + name_len].to_vec();
        let value = buf[entry_body_start + name_len..entry_body_end].to_vec();
        out.push(XattrEntry {
            name_index,
            name,
            value,
        });

        // Advance to next entry, padded up to a 4-byte boundary.
        cur = (entry_body_end + 3) & !3;
    }

    Ok((shared, out))
}

/// Parse a single shared-area xattr entry from a byte slice. The entry
/// has the same wire format as an inline `erofs_xattr_entry`: 4-byte
/// header then `name` then `value`. Padding to the next 4-byte boundary
/// is left to the caller -- shared entries are addressed individually by
/// their start offset, so the trailing pad is unobserved.
fn parse_shared_entry(buf: &[u8]) -> Result<XattrEntry> {
    if buf.len() < XATTR_ENTRY_HEADER_SIZE {
        return Err(Error::BadXattr("shared entry header truncated"));
    }
    let name_len = buf[0] as usize;
    let name_index = buf[1];
    let value_size = u16::from_le_bytes(buf[2..4].try_into().unwrap()) as usize;
    let body_start = XATTR_ENTRY_HEADER_SIZE;
    let body_end = body_start
        .checked_add(name_len)
        .and_then(|p| p.checked_add(value_size))
        .ok_or(Error::BadXattr("shared entry size overflow"))?;
    if body_end > buf.len() {
        return Err(Error::BadXattr("shared entry runs past read"));
    }
    Ok(XattrEntry {
        name_index,
        name: buf[body_start..body_start + name_len].to_vec(),
        value: buf[body_start + name_len..body_end].to_vec(),
    })
}

/// Concatenate the namespace prefix and `name`. ACL slots return their
/// canonical full attribute name even when `name` is empty, since
/// `e_name_index == 2|3` carries no on-disk suffix. Custom-prefix
/// indices (high bit set) fall through to a raw-name copy here -- use
/// [`resolve_with_dict`] when a dictionary is available.
pub fn resolve_full_name(name_index: u8, name: &[u8]) -> Vec<u8> {
    let prefix: &[u8] = match name_index {
        ns::RAW => b"",
        ns::USER => b"user.",
        ns::POSIX_ACL_ACCESS => b"system.posix_acl_access",
        ns::POSIX_ACL_DEFAULT => b"system.posix_acl_default",
        ns::TRUSTED => b"trusted.",
        ns::LUSTRE => b"lustre.",
        ns::SECURITY => b"security.",
        _ => b"",
    };
    let mut out = Vec::with_capacity(prefix.len() + name.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(name);
    out
}

/// Resolve a full attribute name using the optional custom-prefix
/// dictionary. When the high bit (`EROFS_XATTR_LONG_PREFIX`) of
/// `name_index` is set, the low 7 bits index `dict`; the resolved name
/// is `<namespace prefix of dict[i].base_index> + dict[i].infix + name`.
/// Otherwise behaves like [`resolve_full_name`].
///
/// Returns `Error::BadXattr("custom xattr prefix index out of range")`
/// if the dictionary index is past the end of `dict`.
///
/// Spec: `linux/fs/erofs/xattr.h::EROFS_XATTR_LONG_PREFIX*` and
/// `erofs_xattr_long_prefix`. Independent implementation, format
/// verified against `mkfs.erofs` (erofs-utils 1.9) output.
pub fn resolve_with_dict(name_index: u8, name: &[u8], dict: &[XattrLongPrefix]) -> Result<Vec<u8>> {
    if name_index & EROFS_XATTR_LONG_PREFIX == 0 {
        return Ok(resolve_full_name(name_index, name));
    }
    let dict_idx = (name_index & EROFS_XATTR_LONG_PREFIX_MASK) as usize;
    let entry = dict
        .get(dict_idx)
        .ok_or(Error::BadXattr("custom xattr prefix index out of range"))?;
    // Resolve the underlying namespace prefix (e.g. "user.") via
    // resolve_full_name with an empty inline name.
    let ns_prefix = resolve_full_name(entry.base_index, &[]);
    let mut out = Vec::with_capacity(ns_prefix.len() + entry.infix.len() + name.len());
    out.extend_from_slice(&ns_prefix);
    out.extend_from_slice(&entry.infix);
    out.extend_from_slice(name);
    Ok(out)
}

/// Read an inode's inline xattr area and parse it. Returns
/// `(shared_indices, inline_entries)`.
pub fn read_inline_xattrs<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    inode: &Inode,
) -> Result<(Vec<u32>, Vec<XattrEntry>)> {
    if inode.xattr_icount == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let start = Inode::iloc(sb, inode.nid) + inode.on_disk_size as u64;
    let end = inode.body_end(sb);
    if end < start {
        return Err(Error::BadXattr("body_end before xattr start"));
    }
    let len = (end - start) as usize;
    let mut buf = vec![0u8; len];
    dev.read_at(start, &mut buf)?;
    parse_inline_xattrs(&buf)
}

/// Fetch each shared-index entry from the shared xattr block area. The
/// shared area starts at `sb.xattr_blkaddr * block_size`; index `i`
/// addresses the entry at `+ i * 4` bytes from that base.
///
/// Each on-disk entry uses the same header format as an inline entry
/// (4-byte header + name + value); padding to 4 bytes is implicit in
/// the index granularity (every entry starts on a 4-byte boundary).
///
/// Reads each entry in two stages: first the 4-byte header to learn the
/// total length, then the body. This keeps the per-entry I/O cost
/// bounded even when the shared block is large.
pub fn read_shared_xattrs<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    shared_indices: &[u32],
) -> Result<Vec<XattrEntry>> {
    if shared_indices.is_empty() {
        return Ok(Vec::new());
    }
    let base = sb.xattr_blkaddr as u64 * sb.block_size();
    let mut out = Vec::with_capacity(shared_indices.len());
    for &idx in shared_indices {
        let off = base
            .checked_add(
                (idx as u64)
                    .checked_mul(4)
                    .ok_or(Error::BadXattr("shared xattr offset overflow"))?,
            )
            .ok_or(Error::BadXattr("shared xattr offset overflow"))?;
        // Read the header to learn the entry's total length.
        let mut header = [0u8; XATTR_ENTRY_HEADER_SIZE];
        dev.read_at(off, &mut header)?;
        let name_len = header[0] as usize;
        let value_size = u16::from_le_bytes(header[2..4].try_into().unwrap()) as usize;
        let total = XATTR_ENTRY_HEADER_SIZE + name_len + value_size;
        let mut buf = vec![0u8; total];
        dev.read_at(off, &mut buf)?;
        out.push(parse_shared_entry(&buf)?);
    }
    Ok(out)
}

/// Read an inode's COMPLETE xattr set: inline entries and any shared
/// entries referenced by the inline area's shared-index suffix. Returns
/// the merged list with inline entries first, shared entries second.
/// Order is preserved within each group; callers that care about a
/// specific ordering should sort by full name after resolution.
pub fn read_all_xattrs<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    inode: &Inode,
) -> Result<Vec<XattrEntry>> {
    let (shared_indices, mut entries) = read_inline_xattrs(dev, sb, inode)?;
    let shared_entries = read_shared_xattrs(dev, sb, &shared_indices)?;
    entries.extend(shared_entries);
    Ok(entries)
}

/// Read the custom-prefix dictionary at `sb.xattr_prefix_start * 4` for
/// `sb.xattr_prefix_count` entries.
///
/// Each dictionary entry on disk: `u16 size` (LE) followed by `size`
/// bytes of `{ base_index: u8, infix: [u8; size - 1] }`, padded to the
/// next 4-byte boundary before the next entry.
///
/// Note `xattr_prefix_start` is a 4-byte-aligned BYTE offset divided by
/// 4 (so the on-disk byte offset of the dictionary is
/// `xattr_prefix_start * 4`), NOT a block address. This was confirmed
/// empirically against `mkfs.erofs` (erofs-utils 1.9) output.
pub fn read_xattr_prefix_dictionary<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
) -> Result<Vec<XattrLongPrefix>> {
    if sb.xattr_prefix_count == 0 {
        return Ok(Vec::new());
    }
    let base = sb.xattr_prefix_start as u64 * 4;
    let mut out = Vec::with_capacity(sb.xattr_prefix_count as usize);
    let mut cursor = base;
    for _ in 0..sb.xattr_prefix_count {
        // Read the 2-byte size header.
        let mut size_buf = [0u8; 2];
        dev.read_at(cursor, &mut size_buf)?;
        let size = u16::from_le_bytes(size_buf) as usize;
        if size == 0 {
            return Err(Error::BadXattr("xattr prefix entry size zero"));
        }
        // Read the body: 1-byte base_index + (size - 1) infix bytes.
        let mut body = vec![0u8; size];
        dev.read_at(cursor + 2, &mut body)?;
        let base_index = body[0];
        let infix = body[1..].to_vec();
        out.push(XattrLongPrefix { base_index, infix });
        // Advance past size header + body, then 4-byte align.
        let consumed = 2 + size as u64;
        cursor += consumed;
        cursor = (cursor + 3) & !3;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append a single entry (header + name + value) to `buf`, padding
    /// the result to a 4-byte boundary.
    fn push_entry(buf: &mut Vec<u8>, name_index: u8, name: &[u8], value: &[u8]) {
        assert!(name.len() <= u8::MAX as usize);
        assert!(value.len() <= u16::MAX as usize);
        buf.push(name.len() as u8);
        buf.push(name_index);
        buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(value);
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    /// Build an inline xattr buffer: 12-byte header + shared indices +
    /// inline entries (in that on-disk order).
    fn make_buf(shared_indices: &[u32], entries: &[(u8, &[u8], &[u8])]) -> Vec<u8> {
        let mut buf = vec![0u8; XATTR_HEADER_SIZE];
        buf[4] = shared_indices.len() as u8;
        for &idx in shared_indices {
            buf.extend_from_slice(&idx.to_le_bytes());
        }
        for (idx, name, val) in entries {
            push_entry(&mut buf, *idx, name, val);
        }
        buf
    }

    #[test]
    fn round_trip_two_entries() {
        let buf = make_buf(
            &[],
            &[(ns::USER, b"color", b"red"), (ns::TRUSTED, b"foo", b"bar")],
        );
        let (shared, entries) = parse_inline_xattrs(&buf).unwrap();
        assert!(shared.is_empty());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name_index, ns::USER);
        assert_eq!(entries[0].name, b"color");
        assert_eq!(entries[0].value, b"red");
        assert_eq!(entries[1].name_index, ns::TRUSTED);
        assert_eq!(entries[1].name, b"foo");
        assert_eq!(entries[1].value, b"bar");
    }

    #[test]
    fn empty_buffer_yields_empty_list() {
        // icount==0 case. Documented behaviour: accept and return empty.
        let (shared, entries) = parse_inline_xattrs(&[]).unwrap();
        assert!(shared.is_empty());
        assert!(entries.is_empty());
    }

    #[test]
    fn rejects_short_buffer_with_partial_header() {
        let buf = vec![0u8; XATTR_HEADER_SIZE - 1];
        assert!(matches!(parse_inline_xattrs(&buf), Err(Error::BadXattr(_))));
    }

    #[test]
    fn padding_alignment_between_entries() {
        // Lengths chosen so the first entry's (name + value) is NOT a
        // multiple of 4: name=5, value=1 -> 4 bytes header + 6 bytes body
        // = 10 bytes total, padded to 12. Pick a 1+2 second entry.
        let buf = make_buf(
            &[],
            &[(ns::USER, b"abcde", b"x"), (ns::SECURITY, b"k", b"vv")],
        );
        // Header(12) + entry1: 4(hdr)+5(name)+1(val)=10, padded to 12
        //                  -> 12 + 12 = 24. entry2: 4(hdr)+1+2=7, padded to 8.
        assert_eq!(buf.len(), 12 + 12 + 8);
        let (shared, entries) = parse_inline_xattrs(&buf).unwrap();
        assert!(shared.is_empty());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b"abcde");
        assert_eq!(entries[0].value, b"x");
        assert_eq!(entries[1].name, b"k");
        assert_eq!(entries[1].value, b"vv");
    }

    #[test]
    fn shared_indices_returned_in_order() {
        // 2 shared indices, 1 inline entry.
        let buf = make_buf(&[42, 17], &[(ns::USER, b"a", b"1")]);
        let (shared, entries) = parse_inline_xattrs(&buf).unwrap();
        assert_eq!(shared, vec![42, 17]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, b"a");
    }

    #[test]
    fn rejects_entry_running_past_area() {
        let mut buf = vec![0u8; XATTR_HEADER_SIZE];
        // Claim a name_len of 200 with no body.
        buf.extend_from_slice(&[200, ns::USER, 0, 0]);
        assert!(matches!(parse_inline_xattrs(&buf), Err(Error::BadXattr(_))));
    }

    #[test]
    fn rejects_shared_count_overlapping_buffer_end() {
        // Header alone (12 bytes) but shared_count claims 4 slots.
        let mut buf = vec![0u8; XATTR_HEADER_SIZE];
        buf[4] = 4;
        assert!(matches!(parse_inline_xattrs(&buf), Err(Error::BadXattr(_))));
    }

    #[test]
    fn resolve_full_name_user_prefix() {
        let full = resolve_full_name(ns::USER, b"color");
        assert_eq!(full, b"user.color");
    }

    #[test]
    fn resolve_full_name_acl_access_uses_canonical() {
        let full = resolve_full_name(ns::POSIX_ACL_ACCESS, b"");
        assert_eq!(full, b"system.posix_acl_access");
    }

    #[test]
    fn resolve_full_name_raw_passthrough() {
        let full = resolve_full_name(ns::RAW, b"odd.thing");
        assert_eq!(full, b"odd.thing");
    }

    #[test]
    fn resolve_full_name_trusted_security_lustre() {
        assert_eq!(resolve_full_name(ns::TRUSTED, b"x"), b"trusted.x");
        assert_eq!(resolve_full_name(ns::SECURITY, b"x"), b"security.x");
        assert_eq!(resolve_full_name(ns::LUSTRE, b"x"), b"lustre.x");
    }

    #[test]
    fn resolve_with_dict_passes_through_builtin_namespaces() {
        // Indexes 0..=6 should round-trip identically to resolve_full_name.
        let dict: Vec<XattrLongPrefix> = Vec::new();
        for idx in 0u8..=6 {
            assert_eq!(
                resolve_with_dict(idx, b"name", &dict).unwrap(),
                resolve_full_name(idx, b"name"),
                "index {idx}"
            );
        }
    }

    #[test]
    fn resolve_with_dict_uses_dictionary_for_high_bit_indices() {
        // Two prefixes:
        //   dict[0] = USER + "dataitem"  -> name_index 0x80, name ".thing"
        //                                    yields "user.dataitem.thing"
        //   dict[1] = TRUSTED + "config" -> name_index 0x81, name ".foo"
        //                                    yields "trusted.config.foo"
        let dict = vec![
            XattrLongPrefix {
                base_index: ns::USER,
                infix: b"dataitem".to_vec(),
            },
            XattrLongPrefix {
                base_index: ns::TRUSTED,
                infix: b"config".to_vec(),
            },
        ];
        assert_eq!(
            resolve_with_dict(0x80, b".thing", &dict).unwrap(),
            b"user.dataitem.thing"
        );
        assert_eq!(
            resolve_with_dict(0x81, b".foo", &dict).unwrap(),
            b"trusted.config.foo"
        );
    }

    #[test]
    fn resolve_with_dict_rejects_out_of_range_index() {
        let dict = vec![XattrLongPrefix {
            base_index: ns::USER,
            infix: b"x".to_vec(),
        }];
        // dict has 1 entry (index 0); name_index 0x81 -> dict_idx 1 -> out of range.
        let err = resolve_with_dict(0x81, b"foo", &dict).unwrap_err();
        assert!(matches!(err, Error::BadXattr(_)));
        // Empty dict + any high-bit name_index: out of range.
        let err = resolve_with_dict(0x80, b"foo", &[]).unwrap_err();
        assert!(matches!(err, Error::BadXattr(_)));
    }

    /// In-memory `BlockRead` for unit tests in this module. Tiny and
    /// dependency-free so we can synthesize shared-block / dictionary
    /// images without spinning up the full `mkfs` writer.
    struct MemDev(Vec<u8>);
    impl BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let s = offset as usize;
            let e = s + buf.len();
            if e > self.0.len() {
                return Err(fs_core::Error::ShortRead {
                    offset,
                    want: buf.len(),
                    got: self.0.len().saturating_sub(s),
                });
            }
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// Build a synthetic image whose shared-xattr block area at byte 0
    /// (xattr_blkaddr = 0) holds two shared entries. Returns the device
    /// and the indices for each.
    fn synth_shared_image() -> (MemDev, [u32; 2]) {
        let mut img = vec![0u8; 256];
        // Entry 0 at byte offset 16 (idx = 4): user.color = "red"
        // header: name_len=5, name_idx=USER, val_size=3, then "color" + "red"
        let off0 = 16u32;
        img[off0 as usize] = 5; // name_len
        img[off0 as usize + 1] = ns::USER; // name_index
        img[off0 as usize + 2..off0 as usize + 4].copy_from_slice(&3u16.to_le_bytes());
        img[off0 as usize + 4..off0 as usize + 9].copy_from_slice(b"color");
        img[off0 as usize + 9..off0 as usize + 12].copy_from_slice(b"red");
        // Entry 1 at byte offset 32 (idx = 8): trusted.foo = "bar"
        let off1 = 32u32;
        img[off1 as usize] = 3; // name_len
        img[off1 as usize + 1] = ns::TRUSTED;
        img[off1 as usize + 2..off1 as usize + 4].copy_from_slice(&3u16.to_le_bytes());
        img[off1 as usize + 4..off1 as usize + 7].copy_from_slice(b"foo");
        img[off1 as usize + 7..off1 as usize + 10].copy_from_slice(b"bar");
        (MemDev(img), [off0 / 4, off1 / 4])
    }

    /// Build a Superblock with `xattr_blkaddr = 0` (so shared offset =
    /// `idx * 4`) and a 4 KiB block size (irrelevant -- index math
    /// doesn't touch block size when blkaddr = 0).
    fn synth_sb_for_shared() -> Superblock {
        let buf = crate::superblock::tests::synth_sb(12, 0, 0, 1);
        Superblock::parse(&buf).unwrap()
    }

    #[test]
    fn read_shared_xattrs_two_entries() {
        let (dev, [idx0, idx1]) = synth_shared_image();
        let sb = synth_sb_for_shared();
        let entries = read_shared_xattrs(&dev, &sb, &[idx0, idx1]).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name_index, ns::USER);
        assert_eq!(entries[0].name, b"color");
        assert_eq!(entries[0].value, b"red");
        assert_eq!(entries[1].name_index, ns::TRUSTED);
        assert_eq!(entries[1].name, b"foo");
        assert_eq!(entries[1].value, b"bar");
    }

    #[test]
    fn read_shared_xattrs_empty_list_returns_empty() {
        let (dev, _) = synth_shared_image();
        let sb = synth_sb_for_shared();
        let entries = read_shared_xattrs(&dev, &sb, &[]).unwrap();
        assert!(entries.is_empty());
    }

    /// Build a synthetic image whose xattr-prefix dictionary at byte 8
    /// (xattr_prefix_start = 2, since 2 * 4 = 8) holds two entries.
    fn synth_dict_image() -> Vec<u8> {
        let mut img = vec![0u8; 64];
        // Entry 0 at byte 8: size=9, base_index=USER, infix="dataitem"
        let off0 = 8usize;
        img[off0..off0 + 2].copy_from_slice(&9u16.to_le_bytes());
        img[off0 + 2] = ns::USER;
        img[off0 + 3..off0 + 11].copy_from_slice(b"dataitem");
        // pad to 4: entry0 occupies 8..19 (11 bytes), pad to byte 20.
        // Entry 1 at byte 20: size=7, base_index=TRUSTED, infix="config"
        let off1 = 20usize;
        img[off1..off1 + 2].copy_from_slice(&7u16.to_le_bytes());
        img[off1 + 2] = ns::TRUSTED;
        img[off1 + 3..off1 + 9].copy_from_slice(b"config");
        img
    }

    #[test]
    fn read_dictionary_two_entries() {
        let img = synth_dict_image();
        let dev = MemDev(img);
        // Hand-build a superblock buffer with xattr_prefix_count=2 and
        // xattr_prefix_start=2 (byte offset 8).
        let mut sb_buf = crate::superblock::tests::synth_sb(12, 0, 0, 1);
        sb_buf[0x5B] = 2;
        sb_buf[0x5C..0x60].copy_from_slice(&2u32.to_le_bytes());
        let sb = Superblock::parse(&sb_buf).unwrap();
        let dict = read_xattr_prefix_dictionary(&dev, &sb).unwrap();
        assert_eq!(dict.len(), 2);
        assert_eq!(dict[0].base_index, ns::USER);
        assert_eq!(dict[0].infix, b"dataitem");
        assert_eq!(dict[1].base_index, ns::TRUSTED);
        assert_eq!(dict[1].infix, b"config");
    }

    #[test]
    fn read_dictionary_empty_when_count_zero() {
        let img = synth_dict_image();
        let dev = MemDev(img);
        let sb_buf = crate::superblock::tests::synth_sb(12, 0, 0, 1);
        let sb = Superblock::parse(&sb_buf).unwrap();
        // count=0 (default) -> empty dict, no read.
        let dict = read_xattr_prefix_dictionary(&dev, &sb).unwrap();
        assert!(dict.is_empty());
    }

    #[test]
    fn shared_xattr_with_custom_prefix_resolves_via_dict() {
        // End-to-end: synth shared image with one entry whose
        // name_index = 0x80 (custom dict idx 0), then resolve via dict.
        let mut img = vec![0u8; 128];
        let off = 16usize;
        img[off] = 6; // name_len
        img[off + 1] = 0x80; // EROFS_XATTR_LONG_PREFIX | 0
        img[off + 2..off + 4].copy_from_slice(&1u16.to_le_bytes()); // val_size=1
        img[off + 4..off + 10].copy_from_slice(b".thing");
        img[off + 10] = b'v';
        let dev = MemDev(img);
        let sb = synth_sb_for_shared();
        let entries = read_shared_xattrs(&dev, &sb, &[(off / 4) as u32]).unwrap();
        assert_eq!(entries.len(), 1);
        let dict = vec![XattrLongPrefix {
            base_index: ns::USER,
            infix: b"dataitem".to_vec(),
        }];
        let full = resolve_with_dict(entries[0].name_index, &entries[0].name, &dict).unwrap();
        assert_eq!(full, b"user.dataitem.thing");
        assert_eq!(entries[0].value, b"v");
    }
}
