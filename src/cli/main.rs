//! `rust-fs-erofs`: the command-line tools for EROFS, one multi-call binary.
//!
//! Installed as `rust-fs-erofs` and linked as each dotted name; see
//! `common` for the dispatch and the output contract every tool shares,
//! and `erofs` for the tools themselves.

// The shared plumbing is a library in waiting (see its module docs): its
// API is whole, and a piece EROFS does not call yet is not dead, it is the
// part another driver's tools will.
#[allow(dead_code)]
mod common;
mod erofs;

use std::process::ExitCode;

static FAMILY: common::Family = common::Family {
    repo: "rust-fs-erofs",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "EROFS tools: build an EROFS image, or work on one directly, without mounting it",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-fs-erofs`",
    ],
    tools: &[erofs::mkfs::TOOL],
};

fn main() -> ExitCode {
    common::main(&FAMILY)
}
