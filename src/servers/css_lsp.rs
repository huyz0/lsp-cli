//! A standalone, Rust-native LSP server for CSS/SCSS/Less, bundled and
//! built alongside `lsp` itself — see `src/servers/json_lsp.rs`'s module
//! doc comment for the shared architecture (bundled `[[bin]]` binary,
//! `lsp-server`/`lsp-types` for the protocol, no download/npm/Node.js
//! runtime dependency).
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
//! Scope: `textDocument/documentSymbol` and a minimal `textDocument/hover`.
//! No diagnostics, no completion, no property-value validation.

use std::collections::HashMap;
use std::error::Error;

use lsp_server::{
    Connection, ErrorCode, ExtractError, Message, Notification, Request, RequestId, Response,
};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
};
use lsp_types::request::{DocumentSymbolRequest, HoverRequest};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, Hover, HoverContents,
    HoverParams, HoverProviderCapability, MarkupContent, MarkupKind, OneOf, Position, Range,
    ServerCapabilities, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("lsp-css-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    eprintln!("lsp-css-lsp starting");
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    };
    let init_params = connection.initialize(serde_json::to_value(capabilities)?)?;
    main_loop(connection, init_params)?;
    io_threads.join()?;
    eprintln!("lsp-css-lsp shutting down");
    Ok(())
}

struct Docs {
    text: HashMap<Uri, String>,
}

fn main_loop(
    connection: Connection,
    _init_params: serde_json::Value,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let mut docs = Docs {
        text: HashMap::new(),
    };

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let req = match extract::<DocumentSymbolRequest>(req) {
                    Ok((id, params)) => {
                        let result = handle_document_symbol(&docs, &params);
                        respond(&connection, id, result)?;
                        continue;
                    }
                    Err(req) => req,
                };
                let req = match extract::<HoverRequest>(req) {
                    Ok((id, params)) => {
                        let result = handle_hover(&docs, &params);
                        respond(&connection, id, result)?;
                        continue;
                    }
                    Err(req) => req,
                };
                let resp = Response::new_err(
                    req.id,
                    ErrorCode::MethodNotFound as i32,
                    format!("Unhandled method: {}", req.method),
                );
                connection.sender.send(Message::Response(resp))?;
            }
            Message::Notification(not) => {
                handle_notification(&mut docs, not);
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn extract<R>(req: Request) -> Result<(RequestId, R::Params), Request>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    match req.extract::<R::Params>(R::METHOD) {
        Ok(v) => Ok(v),
        Err(ExtractError::MethodMismatch(req)) => Err(req),
        Err(ExtractError::JsonError { method, error }) => {
            panic!("malformed params for {method}: {error}")
        }
    }
}

fn respond(
    connection: &Connection,
    id: RequestId,
    result: impl serde::Serialize,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    connection
        .sender
        .send(Message::Response(Response::new_ok(id, result)))?;
    Ok(())
}

fn handle_notification(docs: &mut Docs, not: Notification) {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(not.params) {
                docs.text
                    .insert(params.text_document.uri, params.text_document.text);
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(not.params) {
                if let Some(change) = params.content_changes.into_iter().last() {
                    docs.text.insert(params.text_document.uri, change.text);
                }
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(not.params) {
                docs.text.remove(&params.text_document.uri);
            }
        }
        _ => {}
    }
}

fn parse(text: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_css::LANGUAGE.into())
        .ok()?;
    parser.parse(text, None)
}

/// See json_lsp.rs's identical helper for why this matters: byte-offset
/// tree-sitter `Point`s need real UTF-16-code-unit conversion to be a
/// spec-compliant `Position`, not the char-count approximation this
/// codebase's client side (`locate.rs`) allows itself when talking to
/// *other* people's servers.
fn point_to_position(text: &str, point: tree_sitter::Point) -> Position {
    let line = text.lines().nth(point.row).unwrap_or("");
    let byte_col = point.column.min(line.len());
    let utf16_col: usize = line[..byte_col].chars().map(|c| c.len_utf16()).sum();
    Position {
        line: point.row as u32,
        character: utf16_col as u32,
    }
}

fn node_range(text: &str, node: &tree_sitter::Node) -> Range {
    Range {
        start: point_to_position(text, node.start_position()),
        end: point_to_position(text, node.end_position()),
    }
}

fn position_to_byte(text: &str, pos: Position) -> usize {
    let mut byte_offset = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i as u32 == pos.line {
            let mut utf16_count = 0u32;
            for (byte_idx, c) in line.char_indices() {
                if utf16_count >= pos.character {
                    return byte_offset + byte_idx;
                }
                utf16_count += c.len_utf16() as u32;
            }
            return byte_offset + line.len();
        }
        byte_offset += line.len() + 1;
    }
    text.len()
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

fn at_rule_name(text: &str, node: &tree_sitter::Node) -> String {
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
    text[node.start_byte()..end_byte].trim().to_string()
}

fn find_child_by_kind<'a>(
    node: &tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types; every constructor site sets it None the same way.
fn symbols_for_rule_set(text: &str, rule_set: &tree_sitter::Node) -> Vec<DocumentSymbol> {
    let Some(selectors) = rule_set
        .child_by_field_name("selectors")
        .or_else(|| find_child_by_kind(rule_set, "selectors"))
    else {
        return vec![];
    };

    let mut out = Vec::new();
    let mut cursor = selectors.walk();
    for sel in selectors.named_children(&mut cursor) {
        let name = text[sel.byte_range()].to_string();
        if name.trim().is_empty() {
            continue;
        }
        out.push(DocumentSymbol {
            name,
            detail: None,
            kind: selector_kind(&sel),
            tags: None,
            deprecated: None,
            range: node_range(text, rule_set),
            selection_range: node_range(text, &sel),
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
fn symbols_for_node(text: &str, node: &tree_sitter::Node) -> Vec<DocumentSymbol> {
    match node.kind() {
        "rule_set" => symbols_for_rule_set(text, node),
        "media_statement" | "supports_statement" => {
            let name = at_rule_name(text, node);
            let mut children = Vec::new();
            let mut cursor = node.walk();
            if let Some(block) = node.children(&mut cursor).find(|c| c.kind() == "block") {
                let mut bcursor = block.walk();
                for child in block.named_children(&mut bcursor) {
                    children.extend(symbols_for_node(text, &child));
                }
            }
            if children.is_empty() {
                return vec![];
            }
            vec![DocumentSymbol {
                name,
                detail: None,
                kind: SymbolKind::NAMESPACE,
                tags: None,
                deprecated: None,
                range: node_range(text, node),
                selection_range: node_range(text, node),
                children: Some(children),
            }]
        }
        "keyframes_statement" => {
            let name = at_rule_name(text, node);
            vec![DocumentSymbol {
                name,
                detail: None,
                kind: SymbolKind::NAMESPACE,
                tags: None,
                deprecated: None,
                range: node_range(text, node),
                selection_range: node_range(text, node),
                children: None,
            }]
        }
        _ => vec![],
    }
}

fn handle_document_symbol(docs: &Docs, params: &DocumentSymbolParams) -> DocumentSymbolResponse {
    let uri = &params.text_document.uri;
    let Some(text) = docs.text.get(uri) else {
        return DocumentSymbolResponse::Nested(vec![]);
    };
    let Some(tree) = parse(text) else {
        return DocumentSymbolResponse::Nested(vec![]);
    };
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        out.extend(symbols_for_node(text, &child));
    }
    DocumentSymbolResponse::Nested(out)
}

fn handle_hover(docs: &Docs, params: &HoverParams) -> Option<Hover> {
    let uri = &params.text_document_position_params.text_document.uri;
    let text = docs.text.get(uri)?;
    let tree = parse(text)?;
    let byte = position_to_byte(text, params.text_document_position_params.position);
    let node = tree.root_node().descendant_for_byte_range(byte, byte)?;
    let snippet = &text[node.byte_range()];
    if snippet.trim().is_empty() {
        return None;
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: snippet.to_string(),
        }),
        range: Some(node_range(text, &node)),
    })
}
