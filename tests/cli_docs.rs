//! The man pages and shell completions the binary writes for packaging
//! (`rust-fs-erofs generate man|completions SHARE`): one page per name in
//! its section, each tool's page naming every one of its subcommands, and
//! a zsh, bash and fish completion per name.

mod cli_support;

use cli_support::*;
use std::path::PathBuf;

fn generated(tag: &str) -> PathBuf {
    let share = PathBuf::from(fs_erofs_test_support::temp_path!(
        "cli-docs-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&share);
    let share_arg = share.to_string_lossy().into_owned();
    ok(entry().args(["generate", "man", &share_arg]));
    ok(entry().args(["generate", "completions", &share_arg]));
    share
}

fn section(name: &str) -> u8 {
    if name.starts_with("mkfs.") || name.starts_with("fsck.") {
        8
    } else {
        1
    }
}

#[test]
fn every_name_has_a_man_page_in_its_section_naming_its_subcommands() {
    let share = generated("man");
    let mut names = dotted_names();
    names.push("rust-fs-erofs".to_string());
    for name in &names {
        let s = section(name);
        let page = share.join(format!("man/man{s}/{name}.{s}"));
        let text =
            std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
        assert!(text.contains(".TH "), "{name}: not a man page");
        assert!(text.contains("Examples:"), "{name}: no example");
        // Every subcommand --help lists is named on the page.
        let help = stdout(&ok(tool(name).arg("--help")));
        let verbs: Vec<&str> = help
            .lines()
            .skip_while(|l| !l.starts_with("Commands:"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .filter_map(|l| l.split_whitespace().next())
            .filter(|v| *v != "help")
            .collect();
        for verb in verbs {
            assert!(text.contains(verb), "{name}'s page does not mention {verb}");
        }
    }
    // fs.erofs's subcommands have pages of their own, which its list names.
    for verb in [
        "ls", "stat", "read", "export", "xattr", "write", "mkdir", "rm", "rmdir", "mv", "ln",
        "truncate", "get", "info", "set", "resize",
    ] {
        assert!(
            share.join(format!("man/man1/fs.erofs-{verb}.1")).is_file(),
            "no fs.erofs-{verb}.1"
        );
    }
    let page = std::fs::read_to_string(share.join("man/man1/fs.erofs-ls.1")).unwrap();
    for flag in ["recursive", "max\\-depth", "limit"] {
        assert!(page.contains(flag), "ls man page is missing {flag}");
    }
    for (verb, option) in [
        ("write", "at"),
        ("mkdir", "parents"),
        ("ln", "symbolic"),
        ("export", "replace"),
    ] {
        let page =
            std::fs::read_to_string(share.join(format!("man/man1/fs.erofs-{verb}.1"))).unwrap();
        assert!(page.contains(option), "{verb} man page omits {option}");
        assert!(page.contains("Examples:"), "{verb} man page omits examples");
    }
}

#[test]
fn every_name_has_a_zsh_bash_and_fish_completion() {
    let share = generated("completions");
    let mut names = dotted_names();
    names.push("rust-fs-erofs".to_string());
    for name in &names {
        let zsh = std::fs::read_to_string(share.join(format!("zsh/site-functions/_{name}")))
            .unwrap_or_else(|e| panic!("zsh {name}: {e}"));
        assert!(
            zsh.starts_with(&format!("#compdef {name}")),
            "zsh {name}: {}",
            &zsh[..40.min(zsh.len())]
        );
        let bash =
            std::fs::read_to_string(share.join(format!("bash-completion/completions/{name}")))
                .unwrap_or_else(|e| panic!("bash {name}: {e}"));
        assert!(bash.contains("complete -F"), "bash {name}");
        let fish =
            std::fs::read_to_string(share.join(format!("fish/vendor_completions.d/{name}.fish")))
                .unwrap_or_else(|e| panic!("fish {name}: {e}"));
        assert!(fish.contains(&format!("complete -c {name}")), "fish {name}");
        if name == "fs.erofs" {
            for flag in [
                "recursive",
                "xattr",
                "export",
                "replace",
                "max-depth",
                "limit",
                "rm",
                "rmdir",
                "mv",
                "ln",
                "truncate",
                "at",
                "parents",
                "symbolic",
            ] {
                for completion in [&zsh, &bash, &fish] {
                    assert!(completion.contains(flag), "completion is missing {flag}");
                }
            }
        }
    }
}
