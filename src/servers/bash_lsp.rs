//! A standalone, Rust-native LSP server for Bash/shell scripts, bundled and
//! built alongside `lsp` itself — see `src/servers/json_lsp.rs`'s module
//! doc comment for the shared architecture.
//!
//! Like `html_lsp.rs`, this exists partly to fix a real, documented gap:
//! the npm-installed `bash-language-server` this tool used before returns
//! an empty list for `textDocument/documentSymbol` on real scripts (a
//! genuine server limitation, confirmed live, documented in
//! docs/language-support.md), so `outline` never showed anything for Bash.
//! `lsp-bash-lsp` returns real function symbols instead.
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
//! Parsing is `tree-sitter-bash`. Its grammar (confirmed by dumping a real
//! parse tree's s-expression, not guessed): both `name() { ... }` and
//! `function name { ... }` forms parse to the *same*
//! `function_definition(name: (word), body: ...)` shape, so both are
//! handled identically with no special-casing. Variable assignments are
//! `variable_assignment(name: (variable_name), value: ...)`; a command
//! invocation is `command(name: (command_name (word)), argument: ...)`.

use std::collections::HashMap;
use std::error::Error;

use lsp_server::{
    Connection, ErrorCode, ExtractError, Message, Notification, Request, RequestId, Response,
};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
};
use lsp_types::request::{DocumentSymbolRequest, GotoDefinition, HoverRequest, References};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability, Location,
    MarkupContent, MarkupKind, OneOf, Position, Range, ReferenceParams, ServerCapabilities,
    SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("lsp-bash-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    eprintln!("lsp-bash-lsp starting");
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        ..Default::default()
    };
    let init_params = connection.initialize(serde_json::to_value(capabilities)?)?;
    main_loop(connection, init_params)?;
    io_threads.join()?;
    eprintln!("lsp-bash-lsp shutting down");
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
                        respond(&connection, id, handle_document_symbol(&docs, &params))?;
                        continue;
                    }
                    Err(req) => req,
                };
                let req = match extract::<HoverRequest>(req) {
                    Ok((id, params)) => {
                        respond(&connection, id, handle_hover(&docs, &params))?;
                        continue;
                    }
                    Err(req) => req,
                };
                let req = match extract::<GotoDefinition>(req) {
                    Ok((id, params)) => {
                        respond(&connection, id, handle_definition(&docs, &params))?;
                        continue;
                    }
                    Err(req) => req,
                };
                let req = match extract::<References>(req) {
                    Ok((id, params)) => {
                        respond(&connection, id, handle_references(&docs, &params))?;
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
        .set_language(&tree_sitter_bash::LANGUAGE.into())
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
    fn record_def(&mut self, text: &str, node: tree_sitter::Node<'a>) {
        self.defs.entry(text.to_string()).or_default().push(node);
        self.all.entry(text.to_string()).or_default().push(node);
    }

    fn record_ref(&mut self, text: &str, node: tree_sitter::Node<'a>) {
        self.all.entry(text.to_string()).or_default().push(node);
    }
}

fn build_index<'a>(text: &str, node: tree_sitter::Node<'a>, idx: &mut Index<'a>) {
    match node.kind() {
        "function_definition" => {
            let name_id = node.child_by_field_name("name").map(|n| {
                idx.record_def(&text[n.byte_range()], n);
                n.id()
            });
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if Some(child.id()) != name_id {
                    build_index(text, child, idx);
                }
            }
        }
        "variable_assignment" => {
            let name_id = node.child_by_field_name("name").map(|n| {
                idx.record_def(&text[n.byte_range()], n);
                n.id()
            });
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if Some(child.id()) != name_id {
                    build_index(text, child, idx);
                }
            }
        }
        "command_name" => {
            if let Some(word) = node.named_child(0) {
                idx.record_ref(&text[word.byte_range()], word);
            }
        }
        "variable_name" => {
            idx.record_ref(&text[node.byte_range()], node);
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                build_index(text, child, idx);
            }
        }
    }
}

#[allow(deprecated)] // `DocumentSymbol.deprecated` has no replacement in lsp_types; every constructor site sets it None the same way.
fn symbols_for_node(text: &str, node: &tree_sitter::Node) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                let name = text[name_node.byte_range()].to_string();
                let body_children = find_child_by_kind(&child, "compound_statement")
                    .map(|b| symbols_for_node(text, &b))
                    .unwrap_or_default();
                out.push(DocumentSymbol {
                    name,
                    detail: None,
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    range: node_range(text, &child),
                    selection_range: node_range(text, &name_node),
                    children: if body_children.is_empty() {
                        None
                    } else {
                        Some(body_children)
                    },
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
                        name: text[name_node.byte_range()].to_string(),
                        detail: None,
                        kind: SymbolKind::VARIABLE,
                        tags: None,
                        deprecated: None,
                        range: node_range(text, &child),
                        selection_range: node_range(text, &name_node),
                        children: None,
                    });
                }
            }
            _ => {}
        }
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

/// Finds the identifier text at `byte` — a `word` (function name, inside
/// `command_name`), `variable_name`, or a `function_definition`'s own
/// `name` field — so definition/references can look it up in the index
/// regardless of which kind of node the cursor happens to land on.
fn identifier_at<'a>(text: &str, root: &tree_sitter::Node<'a>, byte: usize) -> Option<String> {
    let node = root.descendant_for_byte_range(byte, byte)?;
    match node.kind() {
        "variable_name" | "word" => Some(text[node.byte_range()].to_string()),
        _ => None,
    }
}

fn handle_definition(docs: &Docs, params: &GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
    let uri = &params.text_document_position_params.text_document.uri;
    let text = docs.text.get(uri)?;
    let tree = parse(text)?;
    let byte = position_to_byte(text, params.text_document_position_params.position);
    let name = identifier_at(text, &tree.root_node(), byte)?;
    let mut idx = Index::default();
    build_index(text, tree.root_node(), &mut idx);
    let defs = idx.defs.get(&name)?;
    if defs.is_empty() {
        return None;
    }
    let locations: Vec<Location> = defs
        .iter()
        .map(|n| Location {
            uri: uri.clone(),
            range: node_range(text, n),
        })
        .collect();
    Some(GotoDefinitionResponse::Array(locations))
}

fn handle_references(docs: &Docs, params: &ReferenceParams) -> Vec<Location> {
    let uri = &params.text_document_position.text_document.uri;
    let Some(text) = docs.text.get(uri) else {
        return vec![];
    };
    let Some(tree) = parse(text) else {
        return vec![];
    };
    let byte = position_to_byte(text, params.text_document_position.position);
    let Some(name) = identifier_at(text, &tree.root_node(), byte) else {
        return vec![];
    };
    let mut idx = Index::default();
    build_index(text, tree.root_node(), &mut idx);
    idx.all
        .get(&name)
        .into_iter()
        .flatten()
        .map(|n| Location {
            uri: uri.clone(),
            range: node_range(text, n),
        })
        .collect()
}
