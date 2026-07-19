mod support;
use support::lsp;

#[test]
fn install_list_shows_all_managed_languages() {
    let result = lsp(&["install", "--list"]);
    assert_eq!(result.exit_code, 0);
    for lang in [
        "typescript",
        "python",
        "go",
        "rust",
        "java",
        "kotlin",
        "html",
        "css",
        "json",
        "cpp",
        "lua",
        "zig",
        "bash",
        "csharp",
        "ruby",
    ] {
        assert!(
            result.stdout.contains(lang),
            "expected {lang} in install --list output:\n{}",
            result.stdout
        );
    }
    // deno relies on PATH rather than being auto-installed, but should
    // still be listed with its detected status.
    assert!(result.stdout.contains("deno"));
}

#[test]
fn install_unknown_language_errors() {
    let result = lsp(&["install", "not-a-real-language"]);
    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.contains("Unknown language"));
}
