mod support;
use support::lsp;

#[test]
fn help_lists_all_commands() {
    let result = lsp(&["--help"]);
    assert_eq!(result.exit_code, 0);
    let stdout = result.stdout.to_lowercase();

    assert!(stdout.contains("usage:"));
    for cmd in ["outline", "definition", "reference", "doc", "symbol", "search", "locate", "install"] {
        assert!(stdout.contains(cmd), "help output missing `{cmd}`");
    }
    assert!(stdout.contains("--help"));
    assert!(stdout.contains("--version"));
}
