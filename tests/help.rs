mod support;
use support::lsp;

/// Every subcommand `src/main.rs` declares.
///
/// This list used to name eight of them, omitting the six most recently
/// added (`rename`, `hierarchy`, `calls`, `diagnostics`, `server`, `mcp`) —
/// so a subcommand could have been dropped from the CLI entirely and CI
/// would have stayed green.
const ALL_COMMANDS: &[&str] = &[
    "outline",
    "definition",
    "reference",
    "doc",
    "diagnostics",
    "calls",
    "hierarchy",
    "rename",
    "symbol",
    "locate",
    "search",
    "install",
    "server",
    "mcp",
    "schema",
];

#[test]
fn help_lists_every_command() {
    let result = lsp(&["--help"]);
    assert_eq!(result.exit_code, 0);
    let stdout = result.stdout.to_lowercase();

    assert!(stdout.contains("usage:"));
    for cmd in ALL_COMMANDS {
        assert!(stdout.contains(cmd), "help output missing `{cmd}`");
    }
    assert!(stdout.contains("--help"));
    assert!(stdout.contains("--version"));
}

#[test]
fn every_command_has_its_own_help() {
    for cmd in ALL_COMMANDS {
        let result = lsp(&[cmd, "--help"]);
        assert_eq!(result.exit_code, 0, "`lsp {cmd} --help` failed");
        assert!(
            result.stdout.contains("Usage:"),
            "`lsp {cmd} --help` printed no usage"
        );
    }
}

#[test]
fn version_reports_the_crate_version_not_a_hardcoded_string() {
    let result = lsp(&["--version"]);
    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected {} in {:?}",
        env!("CARGO_PKG_VERSION"),
        result.stdout
    );
}

#[test]
fn outline_rejects_scope_instead_of_silently_ignoring_it() {
    // `--scope`/`--find` were declared on `outline`, documented in its
    // help, advertised by `lsp schema outline`, and then discarded — so
    // asking for one class's structure returned the whole file with no
    // indication the filter had been dropped. Failing loudly is the point.
    let result = lsp(&["outline", "some-file.ts", "--scope", "User"]);
    assert_ne!(
        result.exit_code, 0,
        "outline should reject --scope, not accept and ignore it"
    );
    assert!(
        result.stderr.contains("--scope") || result.stderr.contains("unexpected"),
        "expected a clap parse error naming the flag, got: {:?}",
        result.stderr
    );
}
