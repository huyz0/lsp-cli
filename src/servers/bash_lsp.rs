//! A standalone, Rust-native LSP server for Bash/shell scripts, bundled and
//! built alongside `lsp` itself — see `src/servers/json_lsp.rs`'s module
//! doc comment for the shared architecture. Everything language-agnostic
//! lives in `lsp::server_common`.
//!
//! Like `html_lsp.rs`, this exists partly to fix a real, documented gap:
//! the npm-installed `bash-language-server` this tool used before returns
//! an empty list for `textDocument/documentSymbol` on real scripts (a
//! genuine server limitation, confirmed live), so `outline` never showed
//! anything for Bash. `lsp-bash-lsp` returns real function symbols instead.
//!
//! Unlike the JSON/CSS/HTML servers, this one also implements
//! `textDocument/definition` and `textDocument/references` — the old
//! `bash-language-server` had those working correctly, and dropping them
//! just to gain outline would be a net regression, not a win. Bash scripts
//! are effectively single-file in scope (no cross-file module system to
//! resolve), which makes a whole-document name index tractable: every
//! function definition, function call, variable assignment, and variable
//! expansion in the file is indexed by name in one pass, then definition/
//! references are direct index lookups.
//!
//! One honest capability regression: hover shows the raw token text at the
//! cursor, not real builtin documentation the way `bash-language-server`'s
//! hover on `echo` used to. That data source doesn't exist here.
//!
//! Parsing is `tree-sitter-bash`. Its grammar (confirmed by dumping a real
//! parse tree's s-expression, not guessed): both `name() { ... }` and
//! `function name { ... }` forms parse to the *same*
//! `function_definition(name: (word), body: ...)` shape, so both are
//! handled identically with no special-casing. Variable assignments are
//! `variable_assignment(name: (variable_name), value: ...)`; a command
//! invocation is `command(name: (command_name (word)), argument: ...)`.

use std::collections::HashMap;
use std::error::Error;

use lsp::server_common::{find_child_by_kind, run, Document, Server, MAX_SYMBOL_DEPTH};
use lsp_types::{DocumentSymbol, GotoDefinitionResponse, Location, SymbolKind, Uri};

struct BashServer;

impl Server for BashServer {
    fn name(&self) -> &'static str {
        "lsp-bash-lsp"
    }

    fn language(&self) -> tree_sitter::Language {
        tree_sitter_bash::LANGUAGE.into()
    }

    fn supports_navigation(&self) -> bool {
        true
    }

    fn document_symbols(&self, doc: &Document) -> Vec<DocumentSymbol> {
        let Some(tree) = doc.tree.as_ref() else {
            return vec![];
        };
        symbols_for_node(doc, &tree.root_node(), 0)
    }

    fn definition(&self, doc: &Document, uri: &Uri, byte: usize) -> Option<GotoDefinitionResponse> {
        let tree = doc.tree.as_ref()?;
        let name = identifier_at(doc, &tree.root_node(), byte)?;
        let mut idx = Index::default();
        build_index(doc, tree.root_node(), &mut idx);
        let defs = idx.defs.get(&name)?;
        if defs.is_empty() {
            return None;
        }
        Some(GotoDefinitionResponse::Array(
            defs.iter()
                .map(|n| Location {
                    uri: uri.clone(),
                    range: doc.node_range(n),
                })
                .collect(),
        ))
    }

    fn references(
        &self,
        doc: &Document,
        uri: &Uri,
        byte: usize,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some(tree) = doc.tree.as_ref() else {
            return vec![];
        };
        let Some(name) = identifier_at(doc, &tree.root_node(), byte) else {
            return vec![];
        };
        let mut idx = Index::default();
        build_index(doc, tree.root_node(), &mut idx);

        // Honour the request's `includeDeclaration`. This used to always
        // return everything in `all`, definitions included, even though
        // `commands.rs` asks for `includeDeclaration: false` — so `lsp
        // reference` on a bash function always listed the function's own
        // definition among its usages.
        let declarations: Vec<usize> = if include_declaration {
            Vec::new()
        } else {
            idx.defs
                .get(&name)
                .map(|d| d.iter().map(|n| n.id()).collect())
                .unwrap_or_default()
        };

        idx.all
            .get(&name)
            .into_iter()
            .flatten()
            .filter(|n| !declarations.contains(&n.id()))
            .map(|n| Location {
                uri: uri.clone(),
                range: doc.node_range(n),
            })
            .collect()
    }
}

/// Whole-document name index, built fresh per request (bash scripts are
/// small enough that this is cheap, and it keeps the server stateless
/// between edits rather than needing incremental index maintenance).
/// `defs` holds only true declaration sites (a function's own name, a
/// variable's assignment target) — what `textDocument/definition` answers
/// from. `all` holds every occurrence of every name, definitions included —
/// what `textDocument/references` answers from.
#[derive(Default)]
struct Index<'a> {
    defs: HashMap<String, Vec<tree_sitter::Node<'a>>>,
    all: HashMap<String, Vec<tree_sitter::Node<'a>>>,
}

impl<'a> Index<'a> {
    fn record_def(&mut self, name: &str, node: tree_sitter::Node<'a>) {
        self.defs.entry(name.to_string()).or_default().push(node);
        self.all.entry(name.to_string()).or_default().push(node);
    }

    fn record_ref(&mut self, name: &str, node: tree_sitter::Node<'a>) {
        self.all.entry(name.to_string()).or_default().push(node);
    }
}

fn build_index<'a>(doc: &Document, node: tree_sitter::Node<'a>, idx: &mut Index<'a>) {
    match node.kind() {
        "function_definition" | "variable_assignment" => {
            let name_id = node.child_by_field_name("name").map(|n| {
                idx.record_def(doc.slice(&n), n);
                n.id()
            });
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if Some(child.id()) != name_id {
                    build_index(doc, child, idx);
                }
            }
        }
        "command_name" => {
            if let Some(word) = node.named_child(0) {
                idx.record_ref(doc.slice(&word), word);
            }
        }
        "variable_name" => idx.record_ref(doc.slice(&node), node),
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                build_index(doc, child, idx);
            }
        }
    }
}

#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types.
fn symbols_for_node(doc: &Document, node: &tree_sitter::Node, depth: usize) -> Vec<DocumentSymbol> {
    if depth >= MAX_SYMBOL_DEPTH {
        return vec![];
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                let body_children = find_child_by_kind(&child, "compound_statement")
                    .map(|b| symbols_for_node(doc, &b, depth + 1))
                    .unwrap_or_default();
                out.push(DocumentSymbol {
                    name: doc.slice(&name_node).to_string(),
                    detail: None,
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    range: doc.node_range(&child),
                    selection_range: doc.node_range(&name_node),
                    children: (!body_children.is_empty()).then_some(body_children),
                });
            }
            // Only surface top-level assignments in the outline — one inside
            // a function body is local detail, not something worth a
            // top-of-file-style symbol entry, and nesting it under every
            // containing function/if/loop block would add outline noise
            // without adding navigability `definition`/`references` don't
            // already cover.
            "variable_assignment" if node.kind() == "program" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    out.push(DocumentSymbol {
                        name: doc.slice(&name_node).to_string(),
                        detail: None,
                        kind: SymbolKind::VARIABLE,
                        tags: None,
                        deprecated: None,
                        range: doc.node_range(&child),
                        selection_range: doc.node_range(&name_node),
                        children: None,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Finds the identifier text at `byte` — a `word` (function name, inside
/// `command_name`), `variable_name`, or a `function_definition`'s own
/// `name` field — so definition/references can look it up in the index
/// regardless of which kind of node the cursor happens to land on.
fn identifier_at(doc: &Document, root: &tree_sitter::Node, byte: usize) -> Option<String> {
    let node = root.descendant_for_byte_range(byte, byte)?;
    match node.kind() {
        "variable_name" | "word" => Some(doc.slice(&node).to_string()),
        _ => None,
    }
}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    run(BashServer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::for_test(text, BashServer.language())
    }

    fn outline(text: &str) -> Vec<DocumentSymbol> {
        BashServer.document_symbols(&doc(text))
    }

    fn uri() -> Uri {
        "file:///s.sh".parse().unwrap()
    }

    /// Byte offset of the `nth` occurrence of `needle`.
    fn nth_offset(text: &str, needle: &str, nth: usize) -> usize {
        text.match_indices(needle).nth(nth).unwrap().0
    }

    #[test]
    fn both_function_syntaxes_produce_the_same_symbol() {
        let a = outline("greet() {\n  echo hi\n}\n");
        let b = outline("function greet {\n  echo hi\n}\n");
        assert_eq!(a[0].name, "greet");
        assert_eq!(b[0].name, "greet");
        assert_eq!(a[0].kind, SymbolKind::FUNCTION);
        assert_eq!(b[0].kind, SymbolKind::FUNCTION);
    }

    #[test]
    fn top_level_assignments_are_listed_but_local_ones_are_not() {
        let syms = outline("TOP=1\nf() {\n  INNER=2\n}\n");
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["TOP", "f"]);
        assert_eq!(syms[0].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn nested_functions_appear_as_children() {
        let syms = outline("outer() {\n  inner() {\n    echo hi\n  }\n}\n");
        assert_eq!(syms[0].name, "outer");
        assert_eq!(syms[0].children.as_ref().unwrap()[0].name, "inner");
    }

    #[test]
    fn definition_of_a_call_points_at_the_function() {
        let text = "greet() {\n  echo hi\n}\ngreet\n";
        let d = doc(text);
        let at = nth_offset(text, "greet", 1); // the call, not the definition
        let resp = BashServer.definition(&d, &uri(), at).unwrap();
        let GotoDefinitionResponse::Array(locs) = resp else {
            panic!("expected an array response");
        };
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].range.start.line, 0);
    }

    #[test]
    fn references_exclude_the_declaration_when_not_requested() {
        // commands.rs sends includeDeclaration: false; this used to be
        // ignored, so the definition always showed up among the usages.
        let text = "greet() {\n  echo hi\n}\ngreet\ngreet\n";
        let d = doc(text);
        let at = nth_offset(text, "greet", 1);

        let without = BashServer.references(&d, &uri(), at, false);
        assert_eq!(without.len(), 2, "two call sites, no definition");
        assert!(without.iter().all(|l| l.range.start.line > 0));

        let with = BashServer.references(&d, &uri(), at, true);
        assert_eq!(with.len(), 3, "definition included on request");
    }

    #[test]
    fn variable_references_are_found() {
        let text = "NAME=1\necho \"$NAME\"\n";
        let d = doc(text);
        let at = nth_offset(text, "NAME", 1);
        let refs = BashServer.references(&d, &uri(), at, true);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn definition_of_an_unknown_name_is_none() {
        let text = "echo hi\n";
        let d = doc(text);
        assert!(BashServer
            .definition(&d, &uri(), nth_offset(text, "hi", 0))
            .is_none());
    }

    #[test]
    fn malformed_script_does_not_panic() {
        let _ = outline("");
        let _ = outline("f() {");
        let _ = outline("if then fi done }}}");
    }

    #[test]
    fn positions_are_utf16_columns() {
        // The emoji in the comment is 2 UTF-16 units but 1 char.
        let syms = outline("# 😀\ngreet() {\n  echo hi\n}\n");
        assert_eq!(syms[0].selection_range.start.line, 1);
        assert_eq!(syms[0].selection_range.start.character, 0);
    }
}
