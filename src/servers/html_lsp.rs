//! A standalone, Rust-native LSP server for HTML, bundled and built
//! alongside `lsp` itself — see `src/servers/json_lsp.rs`'s module doc
//! comment for the shared architecture. Everything language-agnostic lives
//! in `lsp::server_common`.
//!
//! This one exists to fix a real bug, not just to drop a runtime
//! dependency. The npm-installed `vscode-html-language-server` this tool
//! used before answers `textDocument/documentSymbol` with *flat*
//! `SymbolInformation[]` rather than hierarchical `DocumentSymbol[]`, so
//! `outline` came back empty for every HTML file — a genuine server
//! limitation that could not be worked around from the client side.
//!
//! `lsp-html-lsp` returns real nested symbols instead: `html` →
//! `head`/`body` → elements, recursively, each named `tag#id.class`
//! (`h1#greeting`, `div#app.container.main`), matching how browser devtools
//! name elements. Void elements (`<img>`, `<br>`) correctly show as
//! childless leaves, since the grammar represents them the same way as any
//! other element, just without an `end_tag` or children.
//! `<script>`/`<style>` are distinct grammar node kinds
//! (`script_element`/`style_element`) whose body is one opaque `raw_text`
//! node, deliberately left unparsed rather than treated as nested HTML.
//!
//! Scope: `textDocument/documentSymbol` and the shared minimal
//! `textDocument/hover`. No diagnostics, no completion, no
//! attribute-value validation.

use std::error::Error;

use lsp::server_common::{find_child_by_kind, run, Document, Server, MAX_SYMBOL_DEPTH};
use lsp_types::{DocumentSymbol, SymbolKind};

struct HtmlServer;

impl Server for HtmlServer {
    fn name(&self) -> &'static str {
        "lsp-html-lsp"
    }

    fn language(&self) -> tree_sitter::Language {
        tree_sitter_html::LANGUAGE.into()
    }

    fn document_symbols(&self, doc: &Document) -> Vec<DocumentSymbol> {
        let Some(tree) = doc.tree.as_ref() else {
            return vec![];
        };
        symbols_for_node(doc, &tree.root_node(), 0)
    }
}

/// Reads one attribute's value (`id="app"` -> `"app"`) from a `start_tag`
/// node, handling both the quoted (`quoted_attribute_value` wrapping an
/// `attribute_value`) and bare/unquoted shapes the grammar allows.
fn attribute_value(
    doc: &Document,
    start_tag: &tree_sitter::Node,
    attr_name: &str,
) -> Option<String> {
    let mut cursor = start_tag.walk();
    for attr in start_tag
        .children(&mut cursor)
        .filter(|c| c.kind() == "attribute")
    {
        let Some(name_node) = find_child_by_kind(&attr, "attribute_name") else {
            continue;
        };
        if doc.slice(&name_node) != attr_name {
            continue;
        }
        if let Some(quoted) = find_child_by_kind(&attr, "quoted_attribute_value") {
            return Some(
                find_child_by_kind(&quoted, "attribute_value")
                    .map(|v| doc.slice(&v).to_string())
                    .unwrap_or_default(),
            );
        }
        if let Some(value) = find_child_by_kind(&attr, "attribute_value") {
            return Some(doc.slice(&value).to_string());
        }
    }
    None
}

/// `div#app.container.main` style name: tag, then `#id` if present, then
/// `.class` per class token if present — the closest thing HTML elements
/// have to an "identifier", and immediately recognizable to anyone who's
/// used browser devtools.
fn element_name(doc: &Document, node: &tree_sitter::Node, tag_name: &str) -> String {
    let Some(start_tag) = find_child_by_kind(node, "start_tag") else {
        return tag_name.to_string();
    };
    let mut name = tag_name.to_string();
    if let Some(id) = attribute_value(doc, &start_tag, "id") {
        if !id.is_empty() {
            name.push('#');
            name.push_str(&id);
        }
    }
    if let Some(classes) = attribute_value(doc, &start_tag, "class") {
        for class in classes.split_whitespace() {
            name.push('.');
            name.push_str(class);
        }
    }
    name
}

fn is_element_kind(kind: &str) -> bool {
    matches!(kind, "element" | "script_element" | "style_element")
}

#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types.
fn symbols_for_node(doc: &Document, node: &tree_sitter::Node, depth: usize) -> Vec<DocumentSymbol> {
    if depth >= MAX_SYMBOL_DEPTH {
        return vec![];
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if !is_element_kind(child.kind()) {
            continue;
        }
        let Some(start_tag) = find_child_by_kind(&child, "start_tag") else {
            continue;
        };
        let Some(tag_name_node) = find_child_by_kind(&start_tag, "tag_name") else {
            continue;
        };
        let tag_name = doc.slice(&tag_name_node).to_string();
        let children = symbols_for_node(doc, &child, depth + 1);
        out.push(DocumentSymbol {
            name: element_name(doc, &child, &tag_name),
            detail: None,
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            range: doc.node_range(&child),
            selection_range: doc.node_range(&tag_name_node),
            children: (!children.is_empty()).then_some(children),
        });
    }
    out
}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    run(HtmlServer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(text: &str) -> Vec<DocumentSymbol> {
        HtmlServer.document_symbols(&Document::for_test(text, HtmlServer.language()))
    }

    #[test]
    fn elements_nest_the_way_the_document_does() {
        let syms = outline("<html><head><title>T</title></head><body><p>x</p></body></html>");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "html");
        let kids = syms[0].children.as_ref().unwrap();
        let names: Vec<_> = kids.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["head", "body"]);
        assert_eq!(kids[0].children.as_ref().unwrap()[0].name, "title");
    }

    #[test]
    fn elements_are_named_tag_hash_id_dot_class() {
        let syms = outline(r#"<div id="app" class="container main">x</div>"#);
        assert_eq!(syms[0].name, "div#app.container.main");
    }

    #[test]
    fn an_id_alone_or_a_class_alone_both_work() {
        assert_eq!(
            outline(r#"<h1 id="greeting">x</h1>"#)[0].name,
            "h1#greeting"
        );
        assert_eq!(outline(r#"<p class="lead">x</p>"#)[0].name, "p.lead");
    }

    #[test]
    fn unquoted_attribute_values_are_read() {
        assert_eq!(outline("<div id=app>x</div>")[0].name, "div#app");
    }

    #[test]
    fn an_empty_id_is_not_appended() {
        assert_eq!(outline(r#"<div id="">x</div>"#)[0].name, "div");
    }

    #[test]
    fn void_elements_are_childless_leaves() {
        let syms = outline("<div><img src=\"a.png\"><br></div>");
        let kids = syms[0].children.as_ref().unwrap();
        let names: Vec<_> = kids.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["img", "br"]);
        assert!(kids.iter().all(|k| k.children.is_none()));
    }

    #[test]
    fn script_and_style_bodies_are_not_parsed_as_html() {
        let syms = outline("<body><script>var a = '<p>';</script><style>.a{}</style></body>");
        let kids = syms[0].children.as_ref().unwrap();
        let names: Vec<_> = kids.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["script", "style"]);
        assert!(kids.iter().all(|k| k.children.is_none()));
    }

    #[test]
    fn malformed_html_does_not_panic() {
        let _ = outline("");
        let _ = outline("<div><p></div>");
        let _ = outline("<<<>>>");
    }

    #[test]
    fn positions_are_utf16_columns() {
        // The astral character is 2 UTF-16 units, so the `p` tag name sits
        // at column 8 where a char count would say 7.
        let syms = outline("<div>😀<p>x</p></div>");
        let p = &syms[0].children.as_ref().unwrap()[0];
        assert_eq!(p.selection_range.start.character, 8);
    }
}
