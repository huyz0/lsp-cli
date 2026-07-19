//! JSON schema dump for CLI commands, matching commands/schema.ts.

use serde_json::{json, Map, Value};

pub fn schemas() -> Map<String, Value> {
    let scope_props = json!({
        "scope": {"type": "string", "description": "Symbol path or line number/range"},
        "find": {"type": "string", "description": "Text pattern within scope (use <|> for cursor position)"},
        "project": {"type": "string", "description": "Override project root directory"},
    });
    let output_props = json!({
        "output": {"type": "string", "description": "Output format (markdown, json)"},
        "dry-run": {"type": "boolean", "description": "Print LSP request without executing"},
    });
    let pagination_props = json!({
        "max-items": {"type": "number", "description": "Maximum results per page", "default": 20},
        "start-index": {"type": "number", "description": "Pagination offset (0-based)", "default": 0},
        "pagination-id": {"type": "string", "description": "Session ID for stable pagination"},
    });

    fn merge(objs: &[Value]) -> Value {
        let mut m = Map::new();
        for o in objs {
            if let Value::Object(map) = o {
                for (k, v) in map {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(m)
    }

    let mut out = Map::new();
    out.insert(
        "outline".into(),
        json!({
            "title": "lsp outline", "description": "Show file structure (classes, functions, methods)",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "all": {"type": "boolean"}}), scope_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "definition".into(),
        json!({
            "title": "lsp definition", "description": "Navigate to where a symbol is defined",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "mode": {"type": "string", "enum": ["definition", "declaration", "type_definition"]}}), scope_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "reference".into(),
        json!({
            "title": "lsp reference", "description": "Find all usages of a symbol",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "mode": {"type": "string", "enum": ["references", "implementations"]}}), scope_props.clone(), output_props.clone(), pagination_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "doc".into(),
        json!({
            "title": "lsp doc", "description": "View documentation and type signature for a symbol",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}}), scope_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "diagnostics".into(),
        json!({
            "title": "lsp diagnostics", "description": "Report compiler/type-checker errors and warnings for a file",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "project": {"type": "string"}}), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "calls".into(),
        json!({
            "title": "lsp calls", "description": "Find who calls, or is called by, a symbol",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "direction": {"type": "string", "enum": ["incoming", "outgoing"]}}), scope_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "symbol".into(),
        json!({
            "title": "lsp symbol", "description": "Get the full source code of the symbol at a location",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}}), scope_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "locate".into(),
        json!({
            "title": "lsp locate", "description": "Verify and resolve a scope+find location in a file",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}}), scope_props.clone(), json!({"output": {"type": "string"}})]),
            "required": ["file"],
        }),
    );
    out.insert(
        "search".into(),
        json!({
            "title": "lsp search", "description": "Search for symbols across the workspace",
            "type": "object",
            "properties": merge(&[json!({"query": {"type": "string"}, "kinds": {"type": "array", "items": {"type": "string"}}, "project": {"type": "string"}}), output_props.clone(), pagination_props.clone()]),
            "required": ["query"],
        }),
    );
    out.insert(
        "install".into(),
        json!({
            "title": "lsp install", "description": "Install or update a language server",
            "type": "object",
            "properties": {
                "language": {"type": "string", "description": "Language to install (e.g. typescript)"},
                "list": {"type": "boolean", "description": "List all language servers and their install status"},
                "update": {"type": "boolean", "description": "Update existing installation"},
            },
        }),
    );
    out.insert(
        "server".into(),
        json!({
            "title": "lsp server", "description": "Manage background LSP server processes",
            "type": "object",
            "properties": {
                "subcommand": {"type": "string", "enum": ["list", "start", "stop", "shutdown"]},
                "path": {"type": "string"},
                "all": {"type": "boolean"},
                "output": {"type": "string"},
            },
        }),
    );
    out
}
