//! A standalone, Rust-native LSP server for HTML, bundled and built
//! alongside `lsp` itself — see `src/servers/json_lsp.rs`'s module doc
//! comment for the shared architecture.
//!
//! This is the one that exists to fix a real gap, not just drop a runtime
//! dependency: the npm-installed `vscode-html-language-server` this tool
//! used before returns *flat* `SymbolInformation[]` for
//! `textDocument/documentSymbol` instead of hierarchical `DocumentSymbol[]`
//! (documented in docs/language-support.md) — the outline command only
//! ever deserializes the hierarchical shape, so outline came back empty
//! for every HTML file, not a bug in this tool's client code but a real
//! server limitation there was no way to work around from the client side.
//! Since this tool now controls both ends of the protocol, that gap simply
//! doesn't exist here: each DOM element becomes a real nested
//! `DocumentSymbol` with its actual children.
//!
//! Parsing is `tree-sitter-html`. Its grammar (confirmed by dumping a real
//! parse tree's s-expression, not guessed): ordinary elements are
//! `element(start_tag(tag_name, attribute...), <children>, end_tag)`; void
//! elements like `<img>`/`<br>` are the same `element` shape but with no
//! `end_tag` and no children; `<script>`/`<style>` are distinct node kinds
//! (`script_element`/`style_element`) whose body is one opaque `raw_text`
//! node, not further-parsed HTML.
//!
//! Scope: `textDocument/documentSymbol` and a minimal `textDocument/hover`.
//! No diagnostics, no completion, no attribute-value validation.

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
        println!("lsp-html-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    eprintln!("lsp-html-lsp starting");
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
    eprintln!("lsp-html-lsp shutting down");
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
        .set_language(&tree_sitter_html::LANGUAGE.into())
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

fn find_child_by_kind<'a>(
    node: &tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

/// Reads one attribute's value (`id="app"` -> `"app"`) from a `start_tag`
/// node, handling both the quoted (`quoted_attribute_value` wrapping an
/// `attribute_value`) and bare/unquoted shapes the grammar allows.
fn attribute_value(text: &str, start_tag: &tree_sitter::Node, attr_name: &str) -> Option<String> {
    let mut cursor = start_tag.walk();
    for attr in start_tag
        .children(&mut cursor)
        .filter(|c| c.kind() == "attribute")
    {
        let name_node = find_child_by_kind(&attr, "attribute_name")?;
        if &text[name_node.byte_range()] != attr_name {
            continue;
        }
        if let Some(quoted) = find_child_by_kind(&attr, "quoted_attribute_value") {
            if let Some(value) = find_child_by_kind(&quoted, "attribute_value") {
                return Some(text[value.byte_range()].to_string());
            }
            return Some(String::new());
        }
        if let Some(value) = find_child_by_kind(&attr, "attribute_value") {
            return Some(text[value.byte_range()].to_string());
        }
    }
    None
}

/// `div#app.container.main` style name: tag, then `#id` if present, then
/// `.class` per class token if present — the closest thing HTML elements
/// have to an "identifier", and immediately recognizable to anyone who's
/// used browser devtools.
fn element_name(text: &str, node: &tree_sitter::Node, tag_name: &str) -> String {
    let Some(start_tag) = find_child_by_kind(node, "start_tag") else {
        return tag_name.to_string();
    };
    let mut name = tag_name.to_string();
    if let Some(id) = attribute_value(text, &start_tag, "id") {
        if !id.is_empty() {
            name.push('#');
            name.push_str(&id);
        }
    }
    if let Some(classes) = attribute_value(text, &start_tag, "class") {
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

#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types; every constructor site sets it None the same way.
fn symbols_for_node(text: &str, node: &tree_sitter::Node) -> Vec<DocumentSymbol> {
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
        let tag_name = &text[tag_name_node.byte_range()];
        let name = element_name(text, &child, tag_name);
        let children = symbols_for_node(text, &child);
        out.push(DocumentSymbol {
            name,
            detail: None,
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            range: node_range(text, &child),
            selection_range: node_range(text, &tag_name_node),
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        });
    }
    out
}

fn handle_document_symbol(docs: &Docs, params: &DocumentSymbolParams) -> DocumentSymbolResponse {
    let uri = &params.text_document.uri;
    let Some(text) = docs.text.get(uri) else {
        return DocumentSymbolResponse::Nested(vec![]);
    };
    let Some(tree) = parse(text) else {
        return DocumentSymbolResponse::Nested(vec![]);
    };
    DocumentSymbolResponse::Nested(symbols_for_node(text, &tree.root_node()))
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
