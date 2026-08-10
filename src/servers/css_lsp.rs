//! A standalone, Rust-native LSP server for CSS/SCSS/Less, bundled and
//! built alongside `lsp` itself — see `src/servers/json_lsp.rs`'s module
//! doc comment for the shared architecture (bundled `[[bin]]` binary,
//! `lsp-server`/`lsp-types` for the protocol, no download/npm/Node.js
//! runtime dependency). Everything language-agnostic lives in
//! `lsp::server_common`.
//!
//! Parsing is `tree-sitter-css`. Its grammar shape (confirmed by dumping a
//! real parse tree's s-expression rather than guessed) puts each
//! comma-separated selector in a rule as its own child node under a shared
//! `selectors` node — `.card, #header { ... }` parses as a `rule_set` with
//! a `selectors` node containing one `class_selector` child and one
//! `id_selector` child, not one blob. `symbols_for_rule_set` uses that
//! directly: one `DocumentSymbol` per individual selector, not one per
//! rule, since that's both more granular and matches what the grammar
//! already hands over for free. Declarations (properties) are not nested
//! under selector symbols — a rule's declarations apply to every
//! comma-separated selector jointly, not each one individually, so nesting
//! them under any single selector would misrepresent a shared rule as a
//! duplicated per-selector tree.
//!
//! Scope: `textDocument/documentSymbol` and the shared minimal
//! `textDocument/hover`. No diagnostics, no completion, no property-value
//! validation.

use std::error::Error;

use lsp::server_common::{find_child_by_kind, run, Document, Server, MAX_SYMBOL_DEPTH};
use lsp_types::{DocumentSymbol, SymbolKind};

struct CssServer;

impl Server for CssServer {
    fn name(&self) -> &'static str {
        "lsp-css-lsp"
    }

    fn language(&self) -> tree_sitter::Language {
        tree_sitter_css::LANGUAGE.into()
    }

    fn document_symbols(&self, doc: &Document) -> Vec<DocumentSymbol> {
        let Some(tree) = doc.tree.as_ref() else {
            return vec![];
        };
        let root = tree.root_node();
        let mut out = Vec::new();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            out.extend(symbols_for_node(doc, &child, 0));
        }
        out
    }
}

/// Classifies one selector node (a direct child of a `selectors` node) into
/// an LSP `SymbolKind` — confirmed against a real parse tree, not guessed:
/// `.foo` is `class_selector`, `#foo` is `id_selector`, bare `div` is
/// `tag_name`, and combinators (`div > p.foo`, `a b`, `a + b`, `a ~ b`) wrap
/// their operands rather than being a single flat node, so they fall to the
/// STRUCT default like any other compound/element selector.
fn selector_kind(node: &tree_sitter::Node) -> SymbolKind {
    match node.kind() {
        "class_selector" => SymbolKind::CLASS,
        "id_selector" => SymbolKind::FIELD,
        "tag_name" | "universal_selector" => SymbolKind::STRUCT,
        "pseudo_class_selector" | "pseudo_element_selector" | "attribute_selector" => {
            SymbolKind::KEY
        }
        _ => SymbolKind::STRUCT,
    }
}

fn at_rule_name(doc: &Document, node: &tree_sitter::Node) -> String {
    // `media_statement`/`supports_statement`'s body is a `block` child;
    // `keyframes_statement`'s is a `keyframe_block_list` child instead
    // (confirmed against a real parse tree, not assumed) — without
    // matching that second kind too, a keyframes rule's whole body ends up
    // folded into its own "name" instead of stopping at `@keyframes spin`.
    // Everything up to (not including) whichever container child comes
    // first is the at-rule's own "selector" text.
    let mut cursor = node.walk();
    let mut end_byte = node.end_byte();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "block" | "keyframe_block_list") {
            end_byte = child.start_byte();
            break;
        }
    }
    doc.text[node.start_byte()..end_byte].trim().to_string()
}

#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types.
fn symbols_for_rule_set(doc: &Document, rule_set: &tree_sitter::Node) -> Vec<DocumentSymbol> {
    let Some(selectors) = rule_set
        .child_by_field_name("selectors")
        .or_else(|| find_child_by_kind(rule_set, "selectors"))
    else {
        return vec![];
    };

    let mut out = Vec::new();
    let mut cursor = selectors.walk();
    for sel in selectors.named_children(&mut cursor) {
        let name = doc.slice(&sel).to_string();
        if name.trim().is_empty() {
            continue;
        }
        out.push(DocumentSymbol {
            name,
            detail: None,
            kind: selector_kind(&sel),
            tags: None,
            deprecated: None,
            range: doc.node_range(rule_set),
            selection_range: doc.node_range(&sel),
            children: None,
        });
    }
    out
}

/// Recurses into at-rule blocks (`@media`, `@supports`, ...) so rules
/// nested inside them still show up in the outline, as children of the
/// at-rule's own symbol — `@keyframes`' `from`/`to`/`N%` blocks aren't
/// selector rules at all (a different grammar node, `keyframe_block`), so
/// they're deliberately not descended into; there's nothing selector-like
/// to extract from them.
#[allow(deprecated)]
fn symbols_for_node(doc: &Document, node: &tree_sitter::Node, depth: usize) -> Vec<DocumentSymbol> {
    if depth >= MAX_SYMBOL_DEPTH {
        return vec![];
    }
    match node.kind() {
        "rule_set" => symbols_for_rule_set(doc, node),
        "media_statement" | "supports_statement" => {
            let name = at_rule_name(doc, node);
            let mut children = Vec::new();
            if let Some(block) = find_child_by_kind(node, "block") {
                let mut bcursor = block.walk();
                for child in block.named_children(&mut bcursor) {
                    children.extend(symbols_for_node(doc, &child, depth + 1));
                }
            }
            // An at-rule whose body holds no selector rules (only
            // declarations, or nothing) still belongs in the outline —
            // dropping it hid the `@media` query itself, which is the part
            // a reader is looking for.
            vec![DocumentSymbol {
                name,
                detail: None,
                kind: SymbolKind::NAMESPACE,
                tags: None,
                deprecated: None,
                range: doc.node_range(node),
                selection_range: doc.node_range(node),
                children: (!children.is_empty()).then_some(children),
            }]
        }
        "keyframes_statement" => {
            vec![DocumentSymbol {
                name: at_rule_name(doc, node),
                detail: None,
                kind: SymbolKind::NAMESPACE,
                tags: None,
                deprecated: None,
                range: doc.node_range(node),
                selection_range: doc.node_range(node),
                children: None,
            }]
        }
        _ => vec![],
    }
}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    run(CssServer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(text: &str) -> Vec<DocumentSymbol> {
        CssServer.document_symbols(&Document::for_test(text, CssServer.language()))
    }

    #[test]
    fn each_comma_separated_selector_gets_its_own_symbol() {
        let syms = outline(".card, #header { color: red; }");
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [".card", "#header"]);
        assert_eq!(syms[0].kind, SymbolKind::CLASS);
        assert_eq!(syms[1].kind, SymbolKind::FIELD);
    }

    #[test]
    fn tag_selectors_are_structs() {
        let syms = outline("div { color: red; }");
        assert_eq!(syms[0].name, "div");
        assert_eq!(syms[0].kind, SymbolKind::STRUCT);
    }

    #[test]
    fn media_queries_nest_their_rules() {
        let syms = outline("@media (max-width: 600px) { .a { color: red; } }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "@media (max-width: 600px)");
        assert_eq!(syms[0].kind, SymbolKind::NAMESPACE);
        let kids = syms[0].children.as_ref().unwrap();
        assert_eq!(kids[0].name, ".a");
    }

    #[test]
    fn an_at_rule_with_no_nested_rules_is_still_listed() {
        // It used to be dropped entirely, hiding the query itself.
        let syms = outline("@media print { }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "@media print");
        assert!(syms[0].children.is_none());
    }

    #[test]
    fn keyframes_name_stops_before_its_body() {
        // The keyframes body is `keyframe_block_list`, not `block` — the
        // bug this special case exists for was folding the whole body into
        // the name.
        let syms = outline("@keyframes spin { from { opacity: 0; } to { opacity: 1; } }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "@keyframes spin");
    }

    #[test]
    fn combinator_selectors_stay_one_symbol() {
        let syms = outline("div > p.foo { color: red; }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "div > p.foo");
    }

    #[test]
    fn malformed_css_does_not_panic() {
        let _ = outline("");
        let _ = outline(".a { color:");
        let _ = outline("}}}{{{");
    }

    #[test]
    fn positions_are_utf16_columns() {
        // The emoji in the leading comment is 2 UTF-16 units but 1 char,
        // so `.b` starts at UTF-16 column 9 where a char count gives 8.
        let syms = outline("/* 😀 */ .b { color: red; }");
        assert_eq!(syms[0].selection_range.start.character, 9);
    }
}
