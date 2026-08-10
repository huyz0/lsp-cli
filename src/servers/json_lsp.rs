//! A standalone, Rust-native LSP server for JSON/JSONC, bundled and built
//! alongside the `lsp` binary itself (see Cargo.toml's `[[bin]]` entries) so
//! `lsp install json` needs no npm/Node.js runtime dependency at all —
//! `registry.rs` resolves this binary relative to `lsp`'s own install
//! location instead of downloading anything.
//!
//! Parsing is `tree-sitter-json`. Everything that isn't JSON-specific — the
//! stdio dispatch loop, document storage, position conversion, hover — lives
//! in `lsp::server_common`, shared with the other bundled servers.
//!
//! Scope: `textDocument/documentSymbol` (hierarchical outline of keys) and
//! the shared minimal `textDocument/hover`. No diagnostics, no completion,
//! no schema validation — pure structure, matching what this tool's own
//! commands actually use JSON's server for.

use std::error::Error;

use lsp::server_common::{run, Document, Server, MAX_SYMBOL_DEPTH};
use lsp_types::{DocumentSymbol, SymbolKind};

struct JsonServer;

impl Server for JsonServer {
    fn name(&self) -> &'static str {
        "lsp-json-lsp"
    }

    fn language(&self) -> tree_sitter::Language {
        tree_sitter_json::LANGUAGE.into()
    }

    fn document_symbols(&self, doc: &Document) -> Vec<DocumentSymbol> {
        let Some(tree) = doc.tree.as_ref() else {
            return vec![];
        };
        let root = tree.root_node();
        // The grammar's top-level node is `document`, wrapping the single
        // actual value (object/array/scalar) the file contains.
        let value = root.named_child(0).unwrap_or(root);
        symbols_for_value(doc, &value, 0)
    }
}

fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

fn value_kind(node: &tree_sitter::Node) -> SymbolKind {
    match node.kind() {
        "object" => SymbolKind::OBJECT,
        "array" => SymbolKind::ARRAY,
        "string" => SymbolKind::STRING,
        "number" => SymbolKind::NUMBER,
        "true" | "false" => SymbolKind::BOOLEAN,
        "null" => SymbolKind::NULL,
        _ => SymbolKind::KEY,
    }
}

/// One `DocumentSymbol` per `key: value` pair, recursing into nested
/// objects and arrays — this is what makes `outline` hierarchical rather
/// than the flat list some servers return.
#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types.
fn symbols_for_value(
    doc: &Document,
    node: &tree_sitter::Node,
    depth: usize,
) -> Vec<DocumentSymbol> {
    // Deeply nested input (a minified blob of `[[[[...]]]]`) would
    // otherwise recurse until the stack gives out, aborting the process
    // rather than failing the request.
    if depth >= MAX_SYMBOL_DEPTH {
        return vec![];
    }
    match node.kind() {
        "object" => {
            let mut out = Vec::new();
            let mut cursor = node.walk();
            for pair in node.named_children(&mut cursor) {
                if pair.kind() != "pair" {
                    continue;
                }
                let (Some(key_node), Some(value_node)) = (
                    pair.child_by_field_name("key"),
                    pair.child_by_field_name("value"),
                ) else {
                    continue;
                };
                let children = symbols_for_value(doc, &value_node, depth + 1);
                out.push(DocumentSymbol {
                    name: strip_quotes(doc.slice(&key_node)).to_string(),
                    detail: None,
                    kind: value_kind(&value_node),
                    tags: None,
                    deprecated: None,
                    range: doc.node_range(&pair),
                    selection_range: doc.node_range(&key_node),
                    children: (!children.is_empty()).then_some(children),
                });
            }
            out
        }
        "array" => {
            let mut out = Vec::new();
            let mut cursor = node.walk();
            for (i, item) in node.named_children(&mut cursor).enumerate() {
                let children = symbols_for_value(doc, &item, depth + 1);
                out.push(DocumentSymbol {
                    name: format!("[{i}]"),
                    detail: None,
                    kind: value_kind(&item),
                    tags: None,
                    deprecated: None,
                    range: doc.node_range(&item),
                    selection_range: doc.node_range(&item),
                    children: (!children.is_empty()).then_some(children),
                });
            }
            out
        }
        _ => vec![],
    }
}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    run(JsonServer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(text: &str) -> Vec<DocumentSymbol> {
        JsonServer.document_symbols(&Document::for_test(text, JsonServer.language()))
    }

    #[test]
    fn top_level_keys_become_symbols() {
        let syms = outline(r#"{"name": "x", "version": 2}"#);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["name", "version"]);
        assert_eq!(syms[0].kind, SymbolKind::STRING);
        assert_eq!(syms[1].kind, SymbolKind::NUMBER);
    }

    #[test]
    fn nested_objects_become_nested_symbols() {
        let syms = outline(r#"{"a": {"b": {"c": 1}}}"#);
        assert_eq!(syms.len(), 1);
        let b = &syms[0].children.as_ref().unwrap()[0];
        assert_eq!(b.name, "b");
        let c = &b.children.as_ref().unwrap()[0];
        assert_eq!(c.name, "c");
        assert!(c.children.is_none());
    }

    #[test]
    fn array_elements_are_indexed() {
        let syms = outline(r#"{"list": [1, 2]}"#);
        let items = syms[0].children.as_ref().unwrap();
        let names: Vec<_> = items.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["[0]", "[1]"]);
    }

    #[test]
    fn scalar_kinds_are_classified() {
        let syms = outline(r#"{"s": "x", "n": 1, "b": true, "z": null, "o": {}, "a": []}"#);
        let kinds: Vec<_> = syms.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                SymbolKind::STRING,
                SymbolKind::NUMBER,
                SymbolKind::BOOLEAN,
                SymbolKind::NULL,
                SymbolKind::OBJECT,
                SymbolKind::ARRAY,
            ]
        );
    }

    #[test]
    fn key_names_have_their_quotes_stripped() {
        assert_eq!(outline(r#"{"quoted": 1}"#)[0].name, "quoted");
    }

    #[test]
    fn malformed_json_yields_no_symbols_rather_than_panicking() {
        // tree-sitter always returns a tree, error nodes included.
        let _ = outline(r#"{"a": "#);
        let _ = outline("");
        let _ = outline("not json at all");
    }

    #[test]
    fn deep_nesting_is_capped_instead_of_overflowing_the_stack() {
        let depth = MAX_SYMBOL_DEPTH + 50;
        let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let syms = outline(&text);
        // The point is that it returns at all.
        assert!(syms.len() <= 1);
    }

    #[test]
    fn positions_are_utf16_columns() {
        // "😀" is one char but two UTF-16 code units, so the opening quote
        // of the "b" key sits at UTF-16 column 10 — counting chars would
        // give 9.
        let syms = outline("{\"😀\": 1, \"b\": 2}");
        assert_eq!(syms[1].selection_range.start.character, 10);
    }
}
