use crate::protocol::{
    symbol_kind_name, CallHierarchyItem, Diagnostic, DocumentSymbol, HoverResult, Location,
    TextEdit, TypeHierarchyItem,
};
use serde_json::json;

pub enum OutputFormat {
    Json,
    Markdown,
}

/// Server-supplied URI to a displayable path.
///
/// Delegates to the shared decoder rather than stripping the scheme by
/// hand: rust-analyzer and typescript-language-server both percent-encode
/// what they return, so a project under `~/my project/` used to be printed
/// as `/my%20project/...` — a path the user cannot paste anywhere.
fn uri_to_path(uri: &str) -> String {
    lsp::uri::to_path_string(uri)
}

fn symbol_to_json(sym: &DocumentSymbol) -> serde_json::Value {
    let mut obj = json!({
        "name": sym.name,
        "kind": symbol_kind_name(sym.kind),
        "range": {
            "start": {"line": sym.range.start.line + 1, "character": sym.range.start.character},
            "end": {"line": sym.range.end.line + 1, "character": sym.range.end.character},
        }
    });
    if let Some(detail) = &sym.detail {
        obj["detail"] = json!(detail);
    }
    if let Some(children) = &sym.children {
        if !children.is_empty() {
            obj["children"] = json!(children.iter().map(symbol_to_json).collect::<Vec<_>>());
        }
    }
    obj
}

fn severity_name(severity: Option<u32>) -> &'static str {
    match severity {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "information",
        Some(4) => "hint",
        _ => "unknown",
    }
}

fn icon(kind: u32) -> &'static str {
    use crate::protocol::symbol_kind::{
        CLASS, CONSTANT, CONSTRUCTOR, ENUM, FIELD, FUNCTION, INTERFACE, METHOD, MODULE, NAMESPACE,
        PROPERTY, VARIABLE,
    };
    match kind {
        CLASS => "◆",
        INTERFACE => "◇",
        ENUM => "⊞",
        FUNCTION => "ƒ",
        METHOD => "→",
        CONSTRUCTOR => "✦",
        PROPERTY | FIELD => "·",
        VARIABLE => "○",
        CONSTANT => "■",
        MODULE => "▤",
        NAMESPACE => "▣",
        _ => "·",
    }
}

impl OutputFormat {
    pub fn outline(&self, symbols: &[DocumentSymbol]) -> String {
        match self {
            OutputFormat::Json => {
                json!({"kind": "outline", "items": symbols.iter().map(symbol_to_json).collect::<Vec<_>>()}).to_string()
            }
            OutputFormat::Markdown => render_symbols(symbols, 0),
        }
    }

    pub fn definition(&self, locations: &[Location]) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "definition",
                "locations": locations.iter().map(|l| json!({
                    "uri": uri_to_path(&l.uri),
                    "line": l.range.start.line + 1,
                    "character": l.range.start.character,
                    "endLine": l.range.end.line + 1,
                    "endCharacter": l.range.end.character,
                })).collect::<Vec<_>>()
            })
            .to_string(),
            OutputFormat::Markdown => {
                if locations.is_empty() {
                    return "No definition found.".to_string();
                }
                locations
                    .iter()
                    .map(|l| {
                        format!(
                            "→ {}:{}:{}",
                            uri_to_path(&l.uri),
                            l.range.start.line + 1,
                            l.range.start.character + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    pub fn reference(&self, locations: &[Location]) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "reference",
                "locations": locations.iter().map(|l| json!({
                    "uri": uri_to_path(&l.uri),
                    "line": l.range.start.line + 1,
                    "character": l.range.start.character,
                })).collect::<Vec<_>>()
            })
            .to_string(),
            OutputFormat::Markdown => {
                if locations.is_empty() {
                    return "No references found.".to_string();
                }
                locations
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        format!(
                            "{}. {}:{}",
                            i + 1,
                            uri_to_path(&l.uri),
                            l.range.start.line + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    pub fn hover(&self, result: &HoverResult) -> String {
        let text = result.contents.to_text();
        match self {
            OutputFormat::Json => json!({"kind": "hover", "content": text}).to_string(),
            OutputFormat::Markdown => text,
        }
    }

    pub fn symbol_source(&self, name: &str, kind: u32, source: &str) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "symbol",
                "name": name,
                "symbolKind": symbol_kind_name(kind),
                "source": source,
            })
            .to_string(),
            OutputFormat::Markdown => format!(
                "### {} {} [{}]\n\n```\n{}\n```",
                icon(kind),
                name,
                symbol_kind_name(kind),
                source
            ),
        }
    }

    pub fn diagnostics(&self, diagnostics: &[Diagnostic]) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "diagnostics",
                "items": diagnostics.iter().map(|d| json!({
                    "severity": severity_name(d.severity),
                    "line": d.range.start.line + 1,
                    "character": d.range.start.character,
                    "endLine": d.range.end.line + 1,
                    "endCharacter": d.range.end.character,
                    "message": d.message,
                    "source": d.source,
                    "code": d.code,
                })).collect::<Vec<_>>()
            })
            .to_string(),
            OutputFormat::Markdown => {
                if diagnostics.is_empty() {
                    return "No diagnostics.".to_string();
                }
                diagnostics
                    .iter()
                    .map(|d| {
                        format!(
                            "{}:{}: [{}] {}",
                            d.range.start.line + 1,
                            d.range.start.character + 1,
                            severity_name(d.severity),
                            d.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    pub fn calls(&self, direction: &str, items: &[CallHierarchyItem]) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "calls",
                "direction": direction,
                "items": items.iter().map(|i| json!({
                    "name": i.name,
                    "symbolKind": symbol_kind_name(i.kind),
                    "detail": i.detail,
                    "uri": uri_to_path(&i.uri),
                    "line": i.selection_range.start.line + 1,
                    "character": i.selection_range.start.character,
                })).collect::<Vec<_>>()
            })
            .to_string(),
            OutputFormat::Markdown => {
                if items.is_empty() {
                    return format!("No {direction} calls found.");
                }
                items
                    .iter()
                    .map(|i| {
                        format!(
                            "{} {} — {}:{}",
                            icon(i.kind),
                            i.name,
                            uri_to_path(&i.uri),
                            i.selection_range.start.line + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    pub fn error(&self, message: &str) -> String {
        match self {
            OutputFormat::Json => json!({"kind": "error", "message": message}).to_string(),
            OutputFormat::Markdown => format!("Error: {message}"),
        }
    }

    pub fn hierarchy(&self, direction: &str, items: &[TypeHierarchyItem]) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "hierarchy",
                "direction": direction,
                "items": items.iter().map(|i| json!({
                    "name": i.name,
                    "symbolKind": symbol_kind_name(i.kind),
                    "detail": i.detail,
                    "uri": uri_to_path(&i.uri),
                    "line": i.selection_range.start.line + 1,
                    "character": i.selection_range.start.character,
                })).collect::<Vec<_>>()
            })
            .to_string(),
            OutputFormat::Markdown => {
                if items.is_empty() {
                    return format!("No {direction} found.");
                }
                items
                    .iter()
                    .map(|i| {
                        format!(
                            "{} {} — {}:{}",
                            icon(i.kind),
                            i.name,
                            uri_to_path(&i.uri),
                            i.selection_range.start.line + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    /// `files_with_edits` is `(uri, edits)` pairs collected from a
    /// `WorkspaceEdit`; `skipped_ops` is the count of `documentChanges`
    /// entries that were file operations (create/rename/delete a file)
    /// rather than text edits — this tool only applies text edits, so a
    /// nonzero count here means the rename is incomplete even when
    /// `applied` is true, and callers must be told that explicitly rather
    /// than have it look like a clean success.
    pub fn rename(
        &self,
        new_name: &str,
        applied: bool,
        files_with_edits: &[(String, Vec<TextEdit>)],
        skipped_ops: usize,
    ) -> String {
        let edit_count: usize = files_with_edits.iter().map(|(_, e)| e.len()).sum();
        match self {
            OutputFormat::Json => json!({
                "kind": "rename",
                "newName": new_name,
                "applied": applied,
                "editCount": edit_count,
                "skippedOperations": skipped_ops,
                "files": files_with_edits.iter().map(|(uri, edits)| json!({
                    "uri": uri_to_path(uri),
                    "editCount": edits.len(),
                })).collect::<Vec<_>>(),
            })
            .to_string(),
            OutputFormat::Markdown => {
                if files_with_edits.is_empty() {
                    return "No rename edits.".to_string();
                }
                let mut out = files_with_edits
                    .iter()
                    .map(|(uri, edits)| format!("{} — {} edit(s)", uri_to_path(uri), edits.len()))
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push('\n');
                out.push_str(&if applied {
                    format!("Applied {edit_count} edit(s) across {} file(s), renamed to `{new_name}`.", files_with_edits.len())
                } else {
                    format!("Preview only ({edit_count} edit(s) across {} file(s)) — pass --apply to write these changes.", files_with_edits.len())
                });
                if skipped_ops > 0 {
                    out.push_str(&format!("\n{skipped_ops} file operation(s) (create/rename/delete) were NOT applied — this rename is incomplete."));
                }
                out
            }
        }
    }
}

fn render_symbols(symbols: &[DocumentSymbol], depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let mut lines = Vec::new();
    for sym in symbols {
        let start = sym.range.start.line + 1;
        let end = sym.range.end.line + 1;
        lines.push(format!(
            "{indent}{} {} [{}] (lines {start}–{end})",
            icon(sym.kind),
            sym.name,
            symbol_kind_name(sym.kind)
        ));
        if let Some(children) = &sym.children {
            if !children.is_empty() {
                lines.push(render_symbols(children, depth + 1));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Position, Range};

    #[test]
    fn markdown_definition_reports_no_results() {
        assert_eq!(
            OutputFormat::Markdown.definition(&[]),
            "No definition found."
        );
    }

    #[test]
    fn json_definition_converts_to_1_based_lines() {
        let loc = Location {
            uri: "file:///a.rs".into(),
            range: Range {
                start: Position {
                    line: 4,
                    character: 2,
                },
                end: Position {
                    line: 4,
                    character: 6,
                },
            },
        };
        let out = OutputFormat::Json.definition(&[loc]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["locations"][0]["line"], 5);
        assert_eq!(v["locations"][0]["uri"], "/a.rs");
    }

    #[test]
    fn markdown_reference_numbers_entries() {
        let loc = Location {
            uri: "file:///a.rs".into(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        };
        let out = OutputFormat::Markdown.reference(&[loc.clone(), loc]);
        assert!(out.starts_with("1. /a.rs:1"));
        assert!(out.contains("2. /a.rs:1"));
    }

    // --- rename ------------------------------------------------------
    // `fn rename` is what tells the user whether a rename was applied and
    // whether part of it was silently skipped, and it had no test at all.

    fn text_edit(line: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 3 },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn json_rename_preview_reports_counts_and_that_nothing_was_written() {
        let files = vec![(
            "file:///a.rs".to_string(),
            vec![text_edit(0, "x"), text_edit(2, "x")],
        )];
        let out = OutputFormat::Json.rename("x", false, &files, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["kind"], "rename");
        assert_eq!(v["newName"], "x");
        assert_eq!(v["applied"], false);
        assert_eq!(v["editCount"], 2);
        assert_eq!(v["skippedOperations"], 0);
        assert_eq!(v["files"][0]["editCount"], 2);
    }

    #[test]
    fn json_rename_surfaces_skipped_file_operations() {
        // A rename that also needs to create/rename/delete a file is only
        // partly applied by this tool. Reporting the count is what stops
        // that looking complete when it isn't.
        let files = vec![("file:///a.rs".to_string(), vec![text_edit(0, "x")])];
        let out = OutputFormat::Json.rename("x", true, &files, 2);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["applied"], true);
        assert_eq!(v["skippedOperations"], 2);
    }

    #[test]
    fn markdown_rename_preview_says_it_wrote_nothing() {
        let files = vec![("file:///a.rs".to_string(), vec![text_edit(0, "x")])];
        let out = OutputFormat::Markdown.rename("x", false, &files, 0);
        assert!(out.contains("Preview only"));
        assert!(out.contains("--apply"));
        assert!(!out.contains("Applied"));
    }

    #[test]
    fn markdown_rename_applied_says_so() {
        let files = vec![("file:///a.rs".to_string(), vec![text_edit(0, "x")])];
        let out = OutputFormat::Markdown.rename("x", true, &files, 0);
        assert!(out.contains("Applied 1 edit(s)"));
    }

    #[test]
    fn markdown_rename_warns_that_skipped_operations_leave_it_incomplete() {
        let files = vec![("file:///a.rs".to_string(), vec![text_edit(0, "x")])];
        let out = OutputFormat::Markdown.rename("x", true, &files, 1);
        assert!(out.contains("NOT applied"));
        assert!(out.contains("incomplete"));
    }

    #[test]
    fn rename_paths_are_percent_decoded_for_display() {
        let files = vec![(
            "file:///my%20project/a.rs".to_string(),
            vec![text_edit(0, "x")],
        )];
        let out = OutputFormat::Markdown.rename("x", false, &files, 0);
        assert!(
            out.contains("/my project/a.rs"),
            "expected a decoded path, got: {out}"
        );
    }
}
