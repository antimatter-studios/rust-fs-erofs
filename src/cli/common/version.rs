//! The identifying `--version` line: `<tool> (<crate>) <version>`.
//!
//! One shape for every name the binary answers to, because the question
//! it exists to answer is "is the program PATH found ours?", asked by
//! `doctor` and by every test suite before it trusts a tool.

use super::family::Family;

/// The version clap prints after a command's name: `(<crate>) <version>`,
/// so `mkfs.erofs --version` reads `mkfs.erofs (am-fs-erofs) 0.2.0`.
pub fn clap_version(family: &Family) -> &'static str {
    // clap takes a 'static str; one per process, a few dozen bytes.
    Box::leak(format!("({}) {}", family.crate_name, family.version).into_boxed_str())
}

/// The whole line for `tool`.
pub fn line(family: &Family, tool: &str) -> String {
    format!("{tool} ({}) {}", family.crate_name, family.version)
}

/// What a version line says, if it has our shape.
#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    pub tool: String,
    pub crate_name: String,
    pub version: String,
}

/// Read a `--version` answer: the first line, as `<tool> (<crate>)
/// <version>`. Anything else — another package's banner, a usage message,
/// nothing at all — is `None`.
pub fn parse(output: &str) -> Option<Identity> {
    let first = output.lines().next()?.trim();
    let (tool, rest) = first.split_once(" (")?;
    let (crate_name, version) = rest.split_once(") ")?;
    let well_formed = !tool.is_empty()
        && !tool.contains(char::is_whitespace)
        && !crate_name.is_empty()
        && !crate_name.contains(char::is_whitespace)
        && version.split('.').count() >= 3
        && !version.contains(char::is_whitespace);
    well_formed.then(|| Identity {
        tool: tool.to_string(),
        crate_name: crate_name.to_string(),
        version: version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_shape_is_read_and_others_are_not() {
        assert_eq!(
            parse("mkfs.erofs (am-fs-erofs) 0.2.0\n"),
            Some(Identity {
                tool: "mkfs.erofs".into(),
                crate_name: "am-fs-erofs".into(),
                version: "0.2.0".into(),
            })
        );
        // erofs-utils' banner has our SHAPE, and names its own package,
        // which is how doctor tells it apart: by the crate, not the shape.
        assert_eq!(
            parse("mkfs.erofs (erofs-utils) 1.9.1\n"),
            Some(Identity {
                tool: "mkfs.erofs".into(),
                crate_name: "erofs-utils".into(),
                version: "1.9.1".into(),
            })
        );
        // A banner of another shape, and a usage message, are nobody's.
        assert_eq!(parse("mkfs.erofs 1.8.10"), None);
        assert_eq!(parse("Usage: mkfs.erofs [OPTIONS] FILE SOURCE"), None);
        assert_eq!(parse(""), None);
    }
}
