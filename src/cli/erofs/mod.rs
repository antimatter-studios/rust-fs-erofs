//! The EROFS tools: what this repository fills the shared contract in
//! with. Everything filesystem-specific lives here, and nothing here is
//! plumbing (that is `fs_core::cli`, in rust-fs-core).

pub mod fs;
pub mod mkfs;

/// A UUID in its standard 8-4-4-4-12 form.
pub fn format_uuid(u: &[u8; 16]) -> String {
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
