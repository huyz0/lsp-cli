mod support;
use support::lsp;

/// Commands that must appear in `lsp schema`.
///
/// The schema map is what `lsp mcp`'s `tools/list` is generated from, so a
/// command missing here is a command missing from the MCP surface
/// entirely. The old test asserted only that three keys existed, which
/// would not have noticed ten of them disappearing.
const SCHEMA_COMMANDS: &[&str] = &[
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
];

#[test]
fn schema_with_no_command_returns_every_schema() {
    let result = lsp(&["schema"]);
    assert_eq!(result.exit_code, 0);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    for cmd in SCHEMA_COMMANDS {
        assert!(data.get(cmd).is_some(), "schema missing `{cmd}`");
    }
}

#[test]
fn every_schema_is_a_well_formed_object_schema() {
    let result = lsp(&["schema"]);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    for (name, schema) in data.as_object().unwrap() {
        assert_eq!(schema["type"], "object", "{name}: wrong type");
        assert!(
            schema["properties"].is_object(),
            "{name}: missing properties"
        );
        assert!(
            schema["required"].is_array(),
            "{name}: missing required list"
        );
        assert!(
            schema["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "{name}: missing description (it becomes the MCP tool description)"
        );
    }
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
    assert!(data["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "file"));
}

#[test]
fn outline_schema_does_not_advertise_scope_or_find() {
    // It used to, while `run_outline` ignored them — so an MCP client
    // following the schema sent arguments that did nothing.
    let result = lsp(&["schema", "outline"]);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(data["properties"].get("scope").is_none());
    assert!(data["properties"].get("find").is_none());
}

#[test]
fn locate_schema_does_not_advertise_project() {
    // `lsp locate` has no --project flag, so advertising one sent MCP
    // clients into a guaranteed clap parse error.
    let result = lsp(&["schema", "locate"]);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(data["properties"].get("scope").is_some());
    assert!(data["properties"].get("project").is_none());
}

#[test]
fn install_schema_advertises_all() {
    // `lsp install --all` is documented but was absent from the schema, so
    // it was unreachable through MCP.
    let result = lsp(&["schema", "install"]);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert!(data["properties"].get("all").is_some());
}

#[test]
fn schema_for_unknown_command_errors() {
    let result = lsp(&["schema", "unknown-command-does-not-exist"]);
    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.contains("Unknown command"));
}
