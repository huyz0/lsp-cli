use crate::protocol::{
    symbol_kind_name, CallHierarchyItem, Diagnostic, DocumentSymbol, HoverResult, Location,
    SymbolInformation, TextEdit, TypeHierarchyItem,
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
    /// A resolved `--scope`/`--find` position with surrounding context.
    ///
    /// `run_locate` rendered this inline, as did `search` — the only two of
    /// the twelve commands that did not go through this type, which is why
    /// `format.rs` had no test covering either of them.
    pub fn locate(
        &self,
        file: &std::path::Path,
        line: u32,
        character: u32,
        context_start_line: usize,
        context: &[&str],
    ) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "locate",
                "file": file,
                "line": line + 1,
                "character": character,
                "context": context.iter().enumerate().map(|(i, text)| json!({
                    "line": context_start_line + i + 1,
                    "text": text,
                    "isCursor": (context_start_line + i) as u32 == line,
                })).collect::<Vec<_>>(),
            })
            .to_string(),
            OutputFormat::Markdown => {
                let line_num = line + 1;
                let mut out = format!(
                    "Resolved: {}:{}:{}\n",
                    file.display(),
                    line_num,
                    character + 1
                );
                for (i, text) in context.iter().enumerate() {
                    let n = context_start_line + i + 1;
                    let marker = if n as u32 == line_num {
                        "\u{2192}"
                    } else {
                        " "
                    };
                    out.push_str(&format!("\n{marker} {n:>4} \u{2502} {text}"));
                }
                out
            }
        }
    }

    /// One page of workspace symbol results.
    ///
    /// `total` and `start_index` are reported in JSON so a caller can tell
    /// whether more results exist without comparing counts by hand.
    pub fn search(
        &self,
        query: &str,
        page: &[SymbolInformation],
        total: usize,
        start_index: usize,
        next_start_index: usize,
    ) -> String {
        match self {
            OutputFormat::Json => json!({
                "kind": "search",
                "query": query,
                "items": page.iter().map(|sym| json!({
                    "name": sym.name,
                    "kind": symbol_kind_name(sym.kind),
                    "uri": uri_to_path(&sym.location.uri),
                    "line": sym.location.range.start.line + 1,
                    "containerName": sym.container_name,
                })).collect::<Vec<_>>(),
                "total": total,
                "startIndex": start_index,
            })
            .to_string(),
            OutputFormat::Markdown => {
                if page.is_empty() {
                    return "No matches found.".to_string();
                }
                let mut out = page
                    .iter()
                    .enumerate()
                    .map(|(i, sym)| {
                        format!(
                            "{}. [{}] {}  {}:{}",
                            i + start_index + 1,
                            symbol_kind_name(sym.kind),
                            sym.name,
                            uri_to_path(&sym.location.uri),
                            sym.location.range.start.line + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let remaining = total.saturating_sub(start_index + page.len());
                if remaining > 0 {
                    out.push_str(&format!(
                        "\n\n[{remaining} more — use --start-index {next_start_index} ]"
                    ));
                }
                out
            }
        }
    }

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

    // --- locate / search ----------------------------------------------
    // Both used to render inline in commands.rs, which is why neither had
    // a test here.

    #[test]
    fn json_locate_reports_one_based_lines_and_marks_the_cursor_row() {
        let out = OutputFormat::Json.locate(
            std::path::Path::new("/a/b.ts"),
            /* line (0-based) */ 4,
            /* character */ 6,
            /* context starts at 0-based line */ 3,
            &["three", "four", "five"],
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["kind"], "locate");
        assert_eq!(v["line"], 5, "line should be 1-based");
        assert_eq!(v["character"], 6, "character stays 0-based, as LSP has it");
        assert_eq!(v["context"][0]["line"], 4);
        assert_eq!(v["context"][0]["isCursor"], false);
        assert_eq!(v["context"][1]["isCursor"], true);
        assert_eq!(v["context"][2]["isCursor"], false);
    }

    #[test]
    fn markdown_locate_points_an_arrow_at_the_resolved_line() {
        let out = OutputFormat::Markdown.locate(
            std::path::Path::new("/a/b.ts"),
            4,
            6,
            3,
            &["three", "four", "five"],
        );
        assert!(out.starts_with("Resolved: /a/b.ts:5:7"));
        let arrow_line = out
            .lines()
            .find(|l| l.starts_with('\u{2192}'))
            .expect("expected a marked line");
        assert!(
            arrow_line.contains("four"),
            "arrow on the wrong row: {arrow_line}"
        );
    }

    fn symbol(name: &str, line: u32) -> SymbolInformation {
        SymbolInformation {
            name: name.to_string(),
            kind: crate::protocol::symbol_kind::CLASS,
            location: Location {
                uri: "file:///my%20project/a.ts".into(),
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 5 },
                },
            },
            container_name: None,
        }
    }

    #[test]
    fn json_search_reports_total_and_start_index() {
        // These are what let a caller detect truncation without counting.
        let page = vec![symbol("User", 3)];
        let out = OutputFormat::Json.search("User", &page, 42, 20, 40);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["kind"], "search");
        assert_eq!(v["query"], "User");
        assert_eq!(v["total"], 42);
        assert_eq!(v["startIndex"], 20);
        assert_eq!(v["items"][0]["name"], "User");
        assert_eq!(v["items"][0]["kind"], "class");
        assert_eq!(v["items"][0]["line"], 4);
    }

    #[test]
    fn search_paths_are_percent_decoded() {
        let out = OutputFormat::Json.search("User", &[symbol("User", 0)], 1, 0, 20);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["items"][0]["uri"], "/my project/a.ts");
    }

    #[test]
    fn markdown_search_numbers_results_from_the_page_offset() {
        let page = vec![symbol("A", 0), symbol("B", 1)];
        let out = OutputFormat::Markdown.search("q", &page, 2, 20, 40);
        assert!(out.starts_with("21. [class] A"), "got: {out}");
        assert!(out.contains("22. [class] B"));
    }

    #[test]
    fn markdown_search_says_when_more_results_remain() {
        let out = OutputFormat::Markdown.search("q", &[symbol("A", 0)], 10, 0, 20);
        assert!(out.contains("9 more"), "got: {out}");
        assert!(out.contains("--start-index 20"));
    }

    #[test]
    fn markdown_search_with_no_results_says_so() {
        assert_eq!(
            OutputFormat::Markdown.search("q", &[], 0, 0, 20),
            "No matches found."
        );
    }
}
