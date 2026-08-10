//! JSON schema dump for CLI commands, matching commands/schema.ts.

use serde_json::{json, Map, Value};

pub fn schemas() -> Map<String, Value> {
    // `project` is deliberately separate from scope/find. Bundling it in
    // meant `lsp schema locate` advertised a `--project` flag that
    // `Commands::Locate` does not accept, so an MCP client following the
    // schema sent an argument that fails to parse.
    let scope_props = json!({
        "scope": {"type": "string", "description": "Symbol path or line number/range"},
        "find": {"type": "string", "description": "Text pattern within scope (use <|> for cursor position)"},
    });
    let project_props = json!({
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
            // No scope/find: `outline` describes a whole file. It used to
            // advertise them here while the implementation discarded them,
            // so MCP callers were told to send flags that did nothing.
            "properties": merge(&[json!({"file": {"type": "string"}, "all": {"type": "boolean"}, "project": {"type": "string"}}), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "definition".into(),
        json!({
            "title": "lsp definition", "description": "Navigate to where a symbol is defined",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "mode": {"type": "string", "enum": ["definition", "declaration", "type_definition"]}}), scope_props.clone(), project_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "reference".into(),
        json!({
            "title": "lsp reference", "description": "Find all usages of a symbol",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "mode": {"type": "string", "enum": ["references", "implementations"]}}), scope_props.clone(), project_props.clone(), output_props.clone(), pagination_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "doc".into(),
        json!({
            "title": "lsp doc", "description": "View documentation and type signature for a symbol",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}}), scope_props.clone(), project_props.clone(), output_props.clone()]),
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
            "properties": merge(&[json!({"file": {"type": "string"}, "direction": {"type": "string", "enum": ["incoming", "outgoing"]}}), scope_props.clone(), project_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "hierarchy".into(),
        json!({
            "title": "lsp hierarchy", "description": "Find supertypes or subtypes of a class/interface",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "direction": {"type": "string", "enum": ["subtypes", "supertypes"]}}), scope_props.clone(), project_props.clone(), output_props.clone()]),
            "required": ["file"],
        }),
    );
    out.insert(
        "rename".into(),
        json!({
            "title": "lsp rename", "description": "Rename a symbol across every file that references it. Without \"apply\", only previews the edits.",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}, "new-name": {"type": "string"}, "apply": {"type": "boolean"}}), scope_props.clone(), project_props.clone(), output_props.clone()]),
            "required": ["file", "new-name"],
        }),
    );
    out.insert(
        "symbol".into(),
        json!({
            "title": "lsp symbol", "description": "Get the full source code of the symbol at a location",
            "type": "object",
            "properties": merge(&[json!({"file": {"type": "string"}}), scope_props.clone(), project_props.clone(), output_props.clone()]),
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
                // Documented in the README but previously absent here, so
                // `lsp install --all` was unreachable through MCP.
                "all": {"type": "boolean", "description": "Install every supported language server"},
            },
            "required": [],
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
            "required": [],
        }),
    );
    out
}
