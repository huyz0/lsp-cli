use crate::protocol::{symbol_kind_name, CallHierarchyItem, Diagnostic, DocumentSymbol, HoverResult, Location};
use serde_json::json;

pub enum OutputFormat {
    Json,
    Markdown,
}

fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
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
    use crate::protocol::symbol_kind::{CLASS, CONSTANT, CONSTRUCTOR, ENUM, FIELD, FUNCTION, INTERFACE, METHOD, MODULE, NAMESPACE, PROPERTY, VARIABLE};
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
                    .map(|l| format!("→ {}:{}:{}", uri_to_path(&l.uri), l.range.start.line + 1, l.range.start.character + 1))
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
                    .map(|(i, l)| format!("{}. {}:{}", i + 1, uri_to_path(&l.uri), l.range.start.line + 1))
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
            OutputFormat::Markdown => format!("### {} {} [{}]\n\n```\n{}\n```", icon(kind), name, symbol_kind_name(kind), source),
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
                    .map(|d| format!("{}:{}: [{}] {}", d.range.start.line + 1, d.range.start.character + 1, severity_name(d.severity), d.message))
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
                    .map(|i| format!("{} {} — {}:{}", icon(i.kind), i.name, uri_to_path(&i.uri), i.selection_range.start.line + 1))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Position, Range};

    #[test]
    fn markdown_definition_reports_no_results() {
        assert_eq!(OutputFormat::Markdown.definition(&[]), "No definition found.");
    }

    #[test]
    fn json_definition_converts_to_1_based_lines() {
        let loc = Location {
            uri: "file:///a.rs".into(),
            range: Range { start: Position { line: 4, character: 2 }, end: Position { line: 4, character: 6 } },
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
            range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
        };
        let out = OutputFormat::Markdown.reference(&[loc.clone(), loc]);
        assert!(out.starts_with("1. /a.rs:1"));
        assert!(out.contains("2. /a.rs:1"));
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
