mod support;
use support::lsp;

#[test]
fn schema_with_no_command_returns_all_schemas() {
    let result = lsp(&["schema"]);
    assert_eq!(result.exit_code, 0);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(data.get("outline").is_some());
    assert!(data.get("search").is_some());
    assert!(data.get("install").is_some());
}

#[test]
fn schema_for_specific_command() {
    let result = lsp(&["schema", "outline"]);
    assert_eq!(result.exit_code, 0);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(data["title"], "lsp outline");
    assert_eq!(data["type"], "object");
    assert!(data["properties"].get("file").is_some());
    assert!(data["properties"].get("all").is_some());
    assert!(data["properties"].get("scope").is_some());
    assert!(data["required"].as_array().unwrap().iter().any(|v| v == "file"));
}

#[test]
fn schema_for_unknown_command_errors() {
    let result = lsp(&["schema", "unknown-command-does-not-exist"]);
    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.contains("Unknown command"));
}
