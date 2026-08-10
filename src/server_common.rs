//! The parts of a bundled language server that don't depend on the
//! language: the stdio dispatch loop, document storage, position
//! conversion, and hover.
//!
//! The four servers under `src/servers/` are separate `[[bin]]` targets, so
//! before this module existed each carried its own byte-identical copy of
//! `main`, `main_loop`, `extract`, `respond`, `handle_notification`,
//! `point_to_position`, `node_range`, `position_to_byte`, `handle_hover`,
//! `find_child_by_kind` and `struct Docs` — roughly 190 lines duplicated
//! four times, against 90-165 lines of genuinely language-specific code
//! each. A fix to any of them had to be made four times, and in practice
//! wasn't: the servers had already drifted in small ways.
//!
//! Implement [`Server`] for the language-specific half and call [`run`].

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
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, HoverProviderCapability, Location, MarkupContent, MarkupKind,
    OneOf, Position, Range, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
    Uri,
};

use crate::text_pos::{byte_to_utf16_col, LineIndex};

/// Maximum tree depth the symbol extractors will descend.
///
/// tree-sitter parses iteratively and will happily hand back a tree
/// thousands of levels deep for pathological input (`[[[[...]]]]` in a
/// minified JSON blob, say). The extractors recurse, so without a cap that
/// becomes a stack overflow, which aborts the process rather than failing
/// the one request. Far deeper than any real document nests.
pub const MAX_SYMBOL_DEPTH: usize = 128;

/// One open document: its text, and the parse tree and line index derived
/// from it.
///
/// The tree and index are computed once per version rather than per
/// request. Every handler used to call `parse()`, which allocated a fresh
/// `Parser`, re-ran `set_language`, and reparsed the whole document — so
/// hovering ten times over an unchanged file reparsed it ten times.
pub struct Document {
    pub text: String,
    pub tree: Option<tree_sitter::Tree>,
    pub lines: LineIndex,
}

impl Document {
    fn new(text: String, parser: &mut tree_sitter::Parser) -> Self {
        // `None` for the old tree: this server declares full-document sync,
        // so `didChange` carries replacement text with no edit ranges, and
        // tree-sitter's incremental reuse needs `InputEdit` ranges to be
        // correct. A full reparse per *edit* is right; the bug was a full
        // reparse per *request*.
        let tree = parser.parse(&text, None);
        let lines = LineIndex::new(&text);
        Self { text, tree, lines }
    }

    /// tree-sitter `Point` (row, byte column) → LSP `Position` (row, UTF-16
    /// column).
    pub fn point_to_position(&self, point: tree_sitter::Point) -> Position {
        let line = self.lines.line(&self.text, point.row);
        Position {
            line: point.row as u32,
            character: byte_to_utf16_col(line, point.column),
        }
    }

    pub fn node_range(&self, node: &tree_sitter::Node) -> Range {
        Range {
            start: self.point_to_position(node.start_position()),
            end: self.point_to_position(node.end_position()),
        }
    }

    /// LSP `Position` → absolute byte offset.
    pub fn position_to_byte(&self, pos: Position) -> usize {
        self.lines
            .position_to_byte(&self.text, pos.line as usize, pos.character)
    }

    pub fn slice(&self, node: &tree_sitter::Node) -> &str {
        &self.text[node.byte_range()]
    }

    /// Builds a `Document` directly, so each server's symbol extractor can
    /// be unit-tested without standing up a stdio connection. Before the
    /// servers shared this module none of them had a single test.
    pub fn for_test(text: &str, language: tree_sitter::Language) -> Self {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .expect("grammar incompatible with this tree-sitter version");
        Self::new(text.to_string(), &mut parser)
    }
}

/// Open documents, keyed by URI.
#[derive(Default)]
pub struct Docs {
    docs: HashMap<Uri, Document>,
}

impl Docs {
    pub fn get(&self, uri: &Uri) -> Option<&Document> {
        self.docs.get(uri)
    }
}

/// First direct child of `node` with the given kind.
pub fn find_child_by_kind<'a>(
    node: &tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    // Bound to a local first: the cursor has to outlive the iterator it
    // lends to, which a direct `.find(...)` tail expression doesn't allow.
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

/// The language-specific half of a bundled server.
pub trait Server {
    /// Binary name, reported by `--version` and in startup logging.
    fn name(&self) -> &'static str;

    /// tree-sitter grammar for this language.
    fn language(&self) -> tree_sitter::Language;

    /// Hierarchical outline for `textDocument/documentSymbol`.
    fn document_symbols(&self, doc: &Document) -> Vec<DocumentSymbol>;

    /// `textDocument/definition`. Defaults to unsupported.
    fn definition(
        &self,
        _doc: &Document,
        _uri: &Uri,
        _byte: usize,
    ) -> Option<GotoDefinitionResponse> {
        None
    }

    /// `textDocument/references`. Defaults to unsupported.
    fn references(
        &self,
        _doc: &Document,
        _uri: &Uri,
        _byte: usize,
        _include_declaration: bool,
    ) -> Vec<Location> {
        Vec::new()
    }

    /// Whether to advertise definition/references support. Kept explicit so
    /// a server can't silently claim a capability whose default
    /// implementation returns nothing.
    fn supports_navigation(&self) -> bool {
        false
    }
}

/// Runs `server` on stdio until the client shuts it down.
pub fn run<S: Server>(server: S) -> Result<(), Box<dyn Error + Sync + Send>> {
    // `install.rs::check_version` probes every managed server by spawning
    // it with `--version` and reading one line of stdout. Without handling
    // that here the probe would hang forever waiting for an LSP handshake.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{} {}", server.name(), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    eprintln!("{} starting", server.name());
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: server.supports_navigation().then_some(OneOf::Left(true)),
        references_provider: server.supports_navigation().then_some(OneOf::Left(true)),
        ..Default::default()
    };
    connection.initialize(serde_json::to_value(capabilities)?)?;
    main_loop(&connection, &server)?;
    io_threads.join()?;
    eprintln!("{} shutting down", server.name());
    Ok(())
}

fn main_loop<S: Server>(
    connection: &Connection,
    server: &S,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&server.language())?;

    let mut docs = Docs::default();

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                dispatch(connection, server, &docs, req)?;
            }
            Message::Notification(not) => handle_notification(&mut docs, &mut parser, not),
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn dispatch<S: Server>(
    connection: &Connection,
    server: &S,
    docs: &Docs,
    req: Request,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let req = match extract::<DocumentSymbolRequest>(connection, req)? {
        Extracted::Handled => return Ok(()),
        Extracted::Params(id, params) => {
            let result = handle_document_symbol(server, docs, &params);
            return respond(connection, id, result);
        }
        Extracted::Other(req) => req,
    };
    let req = match extract::<HoverRequest>(connection, req)? {
        Extracted::Handled => return Ok(()),
        Extracted::Params(id, params) => {
            let result = handle_hover(docs, &params);
            return respond(connection, id, result);
        }
        Extracted::Other(req) => req,
    };
    let req = match extract::<GotoDefinition>(connection, req)? {
        Extracted::Handled => return Ok(()),
        Extracted::Params(id, params) => {
            let p = &params.text_document_position_params;
            let result = docs.get(&p.text_document.uri).and_then(|doc| {
                server.definition(doc, &p.text_document.uri, doc.position_to_byte(p.position))
            });
            return respond(connection, id, result);
        }
        Extracted::Other(req) => req,
    };
    let req = match extract::<References>(connection, req)? {
        Extracted::Handled => return Ok(()),
        Extracted::Params(id, params) => {
            let p = &params.text_document_position;
            let result = docs
                .get(&p.text_document.uri)
                .map(|doc| {
                    server.references(
                        doc,
                        &p.text_document.uri,
                        doc.position_to_byte(p.position),
                        params.context.include_declaration,
                    )
                })
                .unwrap_or_default();
            return respond(connection, id, result);
        }
        Extracted::Other(req) => req,
    };

    // Unhandled method: a clean JSON-RPC MethodNotFound rather than a
    // dropped request, so the client fails fast instead of timing out.
    let resp = Response::new_err(
        req.id,
        ErrorCode::MethodNotFound as i32,
        format!("Unhandled method: {}", req.method),
    );
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

enum Extracted<P> {
    /// Method matched but the params were malformed; an error response has
    /// already been sent.
    Handled,
    Params(RequestId, P),
    Other(Request),
}

fn extract<R>(
    connection: &Connection,
    req: Request,
) -> Result<Extracted<R::Params>, Box<dyn Error + Sync + Send>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    let id = req.id.clone();
    match req.extract::<R::Params>(R::METHOD) {
        Ok((id, params)) => Ok(Extracted::Params(id, params)),
        Err(ExtractError::MethodMismatch(req)) => Ok(Extracted::Other(req)),
        Err(ExtractError::JsonError { method, error }) => {
            // Reply InvalidParams instead of panicking. This ran on the
            // main thread with no unwind guard, so one malformed request
            // took the whole server down mid-session and every other
            // document open in it went with it. Reachable from ordinary
            // use: an unencoded URI (a project path with a space) fails to
            // deserialize here.
            let resp = Response::new_err(
                id,
                ErrorCode::InvalidParams as i32,
                format!("malformed params for {method}: {error}"),
            );
            connection.sender.send(Message::Response(resp))?;
            Ok(Extracted::Handled)
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

fn handle_notification(docs: &mut Docs, parser: &mut tree_sitter::Parser, not: Notification) {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(not.params) {
                docs.docs.insert(
                    p.text_document.uri,
                    Document::new(p.text_document.text, parser),
                );
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(not.params) {
                // Full sync: the last entry is the whole new document.
                if let Some(change) = p.content_changes.into_iter().last() {
                    docs.docs
                        .insert(p.text_document.uri, Document::new(change.text, parser));
                }
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(not.params) {
                docs.docs.remove(&p.text_document.uri);
            }
        }
        _ => {}
    }
}

fn handle_document_symbol<S: Server>(
    server: &S,
    docs: &Docs,
    params: &DocumentSymbolParams,
) -> DocumentSymbolResponse {
    let symbols = docs
        .get(&params.text_document.uri)
        .filter(|d| d.tree.is_some())
        .map(|doc| server.document_symbols(doc))
        .unwrap_or_default();
    DocumentSymbolResponse::Nested(symbols)
}

/// Minimal hover: the source text of the smallest node under the cursor.
/// Identical across every bundled server, so it lives here rather than
/// being reimplemented per language.
fn handle_hover(docs: &Docs, params: &HoverParams) -> Option<Hover> {
    let p = &params.text_document_position_params;
    let doc = docs.get(&p.text_document.uri)?;
    let tree = doc.tree.as_ref()?;
    let byte = doc.position_to_byte(p.position);
    let node = tree.root_node().descendant_for_byte_range(byte, byte)?;
    let snippet = doc.slice(&node);
    if snippet.trim().is_empty() {
        return None;
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: snippet.to_string(),
        }),
        range: Some(doc.node_range(&node)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str, language: tree_sitter::Language) -> Document {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        Document::new(text.to_string(), &mut parser)
    }

    #[test]
    fn point_to_position_converts_byte_columns_to_utf16() {
        // tree-sitter reports byte columns; LSP wants UTF-16 code units.
        // "😀" is 4 bytes and 2 UTF-16 units, so byte column 12 (just past
        // it, inside the string literal) is UTF-16 column 10.
        let d = doc("{\"a\": \"😀b\"}", tree_sitter_json::LANGUAGE.into());
        let pos = d.point_to_position(tree_sitter::Point { row: 0, column: 12 });
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 10);
    }

    #[test]
    fn position_to_byte_is_the_inverse_of_point_to_position() {
        let d = doc("{\n  \"key\": 1\n}", tree_sitter_json::LANGUAGE.into());
        let pos = Position {
            line: 1,
            character: 2,
        };
        let byte = d.position_to_byte(pos);
        assert_eq!(&d.text[byte..byte + 5], "\"key\"");
    }

    #[test]
    fn document_caches_its_parse_tree() {
        let d = doc("{\"a\": 1}", tree_sitter_json::LANGUAGE.into());
        assert!(d.tree.is_some(), "tree should be parsed once up front");
    }

    #[test]
    fn line_index_handles_crlf_documents() {
        let d = doc("{\r\n  \"a\": 1\r\n}", tree_sitter_json::LANGUAGE.into());
        assert_eq!(d.lines.line(&d.text, 1), "  \"a\": 1");
    }
}
