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

/// `defaultMaxItems` and `managerTimeout` were parsed from
/// `~/.lsp-cli/config.json`, documented in the README, unit tested — and
/// then never read by anything. These check the wiring end to end, through
/// the real config file the CLI loads.
#[test]
fn default_max_items_from_config_is_actually_applied() {
    let home = support::isolated_home("config-max-items");
    std::fs::write(home.path().join("config.json"), r#"{"defaultMaxItems": 1}"#).unwrap();

    // `lsp schema reference` reports the documented default; the value that
    // matters is the one `--max-items` falls back to, which is observable
    // through search's own JSON (it reports `total` and `startIndex`).
    let result = support::lsp_in(&home, &["search", "e", "--output", "json"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let items = data["items"].as_array().unwrap();
    assert!(
        items.len() <= 1,
        "defaultMaxItems=1 should cap the page at one item, got {}",
        items.len()
    );
}

#[test]
fn a_malformed_config_file_still_leaves_the_cli_usable() {
    let home = support::isolated_home("config-malformed");
    std::fs::write(home.path().join("config.json"), "{ not json").unwrap();
    let result = support::lsp_in(&home, &["install", "--list"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
}
