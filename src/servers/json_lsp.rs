//! A standalone, Rust-native LSP server for JSON/JSONC, bundled and built
//! alongside the `lsp` binary itself (see Cargo.toml's `[[bin]]` entries) so
//! `lsp install json` needs no npm/Node.js runtime dependency at all —
//! `registry.rs` resolves this binary relative to `lsp`'s own install
//! location instead of downloading anything.
//!
//! Built on `tree-sitter-json` for parsing (the same incremental,
//! editor-oriented grammar approach used across all the bundled servers in
//! `src/servers/`) and the `lsp-server`/`lsp-types` crates rust-analyzer
//! itself uses for the JSON-RPC-over-stdio server side of the protocol —
//! `src/lsp_client.rs` elsewhere in this codebase only ever implements the
//! *client* side, this is the first server-side implementation here.
//!
//! Scope for this first version: `textDocument/documentSymbol` (hierarchical
//! outline of keys) and a minimal `textDocument/hover` (shows the value at
//! the cursor). No diagnostics, no completion, no schema validation — pure
//! structure, matching what this tool's own commands actually use JSON's
//! server for today.

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
    // Bundled/versioned together with `lsp` itself (see Cargo.toml), not a
    // separately-installed external dependency, so its version is this
    // crate's own version. install.rs's check_version probes this exactly
    // like every other managed server (spawn with --version, read one line
    // of stdout) — without handling it here first, that probe would hang
    // forever waiting for an LSP handshake on stdin instead.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("lsp-json-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    eprintln!("lsp-json-lsp starting");
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
    eprintln!("lsp-json-lsp shutting down");
    Ok(())
}

struct Docs {
    // Full text kept per open document — this server does full-document
    // sync (TextDocumentSyncKind::FULL), so there's no incremental patching
    // to track, just the latest text handed to us on each didChange.
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
                // Unhandled method: respond with a clean JSON-RPC
                // MethodNotFound rather than dropping the request silently.
                // The CLI client already has a documented, deliberate
                // pattern of surfacing this cleanly (see docs/language-
                // support.md's hierarchy notes) rather than hanging.
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
                // Full sync: the last content_changes entry is the whole
                // new document (no `range` field set), matching the
                // TextDocumentSyncKind::FULL capability declared above.
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
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .ok()?;
    parser.parse(text, None)
}

/// Converts a tree-sitter byte-offset `Point` (row, byte-column-within-row)
/// into an LSP `Position` (row, UTF-16-code-unit-column-within-row) —
/// getting this right matters more here than it does on the client side of
/// this codebase (`locate.rs` approximates with `char` counts, "good
/// enough" for talking to *other* people's spec-compliant servers), since
/// this file *is* the server a real spec-compliant client will talk to.
fn point_to_position(text: &str, point: tree_sitter::Point) -> Position {
    let line = text.lines().nth(point.row).unwrap_or("");
    let byte_col = point.column.min(line.len());
    // `line[..byte_col]` is safe as long as byte_col lands on a char
    // boundary, which it always does here: it's either 0, a line length
    // tree-sitter itself reported, or a node boundary tree-sitter computed
    // from the same source bytes.
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

/// Builds one `DocumentSymbol` per `key: value` pair, recursing into
/// nested objects/arrays as children — this is what makes `outline`
/// hierarchical instead of the flat list some servers return.
#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types; every constructor site sets it None the same way.
fn symbols_for_value(text: &str, node: &tree_sitter::Node) -> Vec<DocumentSymbol> {
    match node.kind() {
        "object" => {
            let mut out = Vec::new();
            let mut cursor = node.walk();
            for pair in node.named_children(&mut cursor) {
                if pair.kind() != "pair" {
                    continue;
                }
                let Some(key_node) = pair.child_by_field_name("key") else {
                    continue;
                };
                let Some(value_node) = pair.child_by_field_name("value") else {
                    continue;
                };
                let name = strip_quotes(&text[key_node.byte_range()]).to_string();
                let children = symbols_for_value(text, &value_node);
                out.push(DocumentSymbol {
                    name,
                    detail: None,
                    kind: value_kind(&value_node),
                    tags: None,
                    deprecated: None,
                    range: node_range(text, &pair),
                    selection_range: node_range(text, &key_node),
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                });
            }
            out
        }
        "array" => {
            let mut out = Vec::new();
            let mut cursor = node.walk();
            for (i, item) in node.named_children(&mut cursor).enumerate() {
                let children = symbols_for_value(text, &item);
                out.push(DocumentSymbol {
                    name: format!("[{i}]"),
                    detail: None,
                    kind: value_kind(&item),
                    tags: None,
                    deprecated: None,
                    range: node_range(text, &item),
                    selection_range: node_range(text, &item),
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                });
            }
            out
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
    // The grammar's top-level node is `document`, wrapping the single
    // actual value (object/array/scalar) the file contains.
    let value = root.named_child(0).unwrap_or(root);
    DocumentSymbolResponse::Nested(symbols_for_value(text, &value))
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
        byte_offset += line.len() + 1; // +1 for the '\n' split() consumed
    }
    text.len()
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
