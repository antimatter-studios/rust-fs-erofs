## Interfaces

| `fs.erofs` reading verbs (`ls`, `ls -R`, `stat`, `read`, `export`, `export --recursive`) on a missing path: exit 1 with `"kind": "not_found"` in the JSON error, as the CLI contract has it | Supported | Unreleased (#251) | #251 | `cli::erofs::fs::tests::a_missing_path_is_the_not_found_kind`, `tests/cli/test-fs.sh`, `test-stat.sh`, `test-ls-recursive.sh` |
