//! `rust-fs-erofs`: the command-line tools for EROFS, one multi-call binary.
//!
//! Installed as `rust-fs-erofs` and linked as each dotted name. The
//! dispatch and the output contract every tool shares are `fs_core::cli`
//! (am-fs-core's `cli` feature); `erofs` is the tools themselves.

mod erofs;

use fs_core::cli;
use std::process::ExitCode;

static FAMILY: cli::Family = cli::Family {
    repo: "rust-fs-erofs",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "EROFS tools: build an EROFS image, or work on one directly, without mounting it",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-fs-erofs`",
    ],
    tools: &[erofs::mkfs::TOOL, erofs::fs::TOOL],
};

fn main() -> ExitCode {
    cli::main(&FAMILY)
}
