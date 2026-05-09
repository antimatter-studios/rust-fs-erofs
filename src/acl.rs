//! POSIX ACL parsing for EROFS xattr values.
//!
//! POSIX ACLs are stored as xattr values under namespace indices 2
//! (access ACL) and 3 (default ACL). The value bytes are a 4-byte
//! version header (`POSIX_ACL_XATTR_VERSION = 2`) followed by an array
//! of 8-byte entries:
//!
//! ```text
//! struct posix_acl_xattr_entry {
//!     __le16 e_tag;     // ACL_USER_OBJ=1, ACL_USER=2, ACL_GROUP_OBJ=4,
//!                       // ACL_GROUP=8, ACL_MASK=16, ACL_OTHER=32
//!     __le16 e_perm;    // bits: r=4, w=2, x=1
//!     __le32 e_id;      // uid for USER, gid for GROUP, 0xFFFFFFFF otherwise
//! }
//! ```
//!
//! Source: `linux/include/uapi/linux/posix_acl_xattr.h`.

use crate::error::{Error, Result};

pub const POSIX_ACL_XATTR_VERSION: u32 = 2;
pub const POSIX_ACL_HEADER_SIZE: usize = 4;
pub const POSIX_ACL_ENTRY_SIZE: usize = 8;

/// `ACL_UNDEFINED_ID` from the kernel (uid_t/gid_t -1).
pub const ACL_UNDEFINED_ID: u32 = 0xFFFFFFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclTag {
    UserObj,  // 0x01: file owner perms
    User,     // 0x02: named user; e_id = uid
    GroupObj, // 0x04: file group perms
    Group,    // 0x08: named group; e_id = gid
    Mask,     // 0x10: max effective perms for User/Group/GroupObj
    Other,    // 0x20: world perms
    Unknown(u16),
}

impl AclTag {
    fn from_le(raw: u16) -> Self {
        match raw {
            0x01 => AclTag::UserObj,
            0x02 => AclTag::User,
            0x04 => AclTag::GroupObj,
            0x08 => AclTag::Group,
            0x10 => AclTag::Mask,
            0x20 => AclTag::Other,
            n => AclTag::Unknown(n),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AclPerm(u16);

impl AclPerm {
    pub fn read(self) -> bool {
        self.0 & 0x4 != 0
    }
    pub fn write(self) -> bool {
        self.0 & 0x2 != 0
    }
    pub fn execute(self) -> bool {
        self.0 & 0x1 != 0
    }
    pub fn raw(self) -> u16 {
        self.0
    }
    /// Render as the conventional `rwx` triplet (`-` for unset bits).
    pub fn render(self) -> String {
        format!(
            "{}{}{}",
            if self.read() { 'r' } else { '-' },
            if self.write() { 'w' } else { '-' },
            if self.execute() { 'x' } else { '-' },
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AclEntry {
    pub tag: AclTag,
    pub perm: AclPerm,
    /// uid for User, gid for Group, `ACL_UNDEFINED_ID` otherwise.
    pub id: u32,
}

/// Parse a POSIX ACL xattr value. Returns the list of entries.
/// `value` must be the full xattr value bytes (header + entries).
pub fn parse(value: &[u8]) -> Result<Vec<AclEntry>> {
    if value.len() < POSIX_ACL_HEADER_SIZE {
        return Err(Error::BadXattr("ACL value shorter than 4-byte header"));
    }
    let version = u32::from_le_bytes(value[0..4].try_into().unwrap());
    if version != POSIX_ACL_XATTR_VERSION {
        return Err(Error::BadXattr("ACL header version != 2"));
    }
    let entries_bytes = &value[POSIX_ACL_HEADER_SIZE..];
    if !entries_bytes.len().is_multiple_of(POSIX_ACL_ENTRY_SIZE) {
        return Err(Error::BadXattr(
            "ACL entries area length not a multiple of 8",
        ));
    }
    let n = entries_bytes.len() / POSIX_ACL_ENTRY_SIZE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * POSIX_ACL_ENTRY_SIZE;
        let tag = AclTag::from_le(u16::from_le_bytes(
            entries_bytes[off..off + 2].try_into().unwrap(),
        ));
        let perm = AclPerm(u16::from_le_bytes(
            entries_bytes[off + 2..off + 4].try_into().unwrap(),
        ));
        let id = u32::from_le_bytes(entries_bytes[off + 4..off + 8].try_into().unwrap());
        out.push(AclEntry { tag, perm, id });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_value(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&POSIX_ACL_XATTR_VERSION.to_le_bytes());
        for (tag, perm, id) in entries {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&perm.to_le_bytes());
            v.extend_from_slice(&id.to_le_bytes());
        }
        v
    }

    #[test]
    fn parse_basic_three_entry_acl() {
        let v = make_value(&[
            (0x01, 0x07, ACL_UNDEFINED_ID), // user_obj rwx
            (0x04, 0x05, ACL_UNDEFINED_ID), // group_obj r-x
            (0x20, 0x04, ACL_UNDEFINED_ID), // other r--
        ]);
        let entries = parse(&v).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0].tag, AclTag::UserObj));
        assert_eq!(entries[0].perm.render(), "rwx");
        assert!(matches!(entries[1].tag, AclTag::GroupObj));
        assert_eq!(entries[1].perm.render(), "r-x");
        assert!(matches!(entries[2].tag, AclTag::Other));
        assert_eq!(entries[2].perm.render(), "r--");
    }

    #[test]
    fn parse_named_user_with_id() {
        let v = make_value(&[
            (0x01, 0x06, ACL_UNDEFINED_ID),
            (0x02, 0x04, 1001),             // user 1001: r--
            (0x10, 0x06, ACL_UNDEFINED_ID), // mask: rw-
            (0x04, 0x00, ACL_UNDEFINED_ID),
            (0x20, 0x00, ACL_UNDEFINED_ID),
        ]);
        let entries = parse(&v).unwrap();
        assert_eq!(entries.len(), 5);
        assert!(matches!(entries[1].tag, AclTag::User));
        assert_eq!(entries[1].id, 1001);
        assert_eq!(entries[1].perm.render(), "r--");
        assert!(matches!(entries[2].tag, AclTag::Mask));
    }

    #[test]
    fn rejects_short_value() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[1, 2, 3]).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let mut v = make_value(&[(0x01, 0x07, ACL_UNDEFINED_ID)]);
        v[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(parse(&v), Err(Error::BadXattr(_))));
    }

    #[test]
    fn rejects_misaligned_entries() {
        let mut v = make_value(&[(0x01, 0x07, ACL_UNDEFINED_ID)]);
        v.push(0xFF); // dangling byte
        assert!(matches!(parse(&v), Err(Error::BadXattr(_))));
    }

    #[test]
    fn perm_bits() {
        let p = AclPerm(0x07);
        assert!(p.read() && p.write() && p.execute());
        assert_eq!(p.render(), "rwx");
        let p = AclPerm(0x05);
        assert!(p.read() && !p.write() && p.execute());
        assert_eq!(p.render(), "r-x");
        let p = AclPerm(0);
        assert_eq!(p.render(), "---");
    }
}
