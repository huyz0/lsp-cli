use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

/// LSP error code for `ContentModified` (server was still processing an
/// earlier change; the spec requires the client to silently retry).
const CONTENT_MODIFIED: i64 = -32801;

/// Standard JSON-RPC error code for "the server doesn't implement this
/// method" — used by `daemon.rs` to detect servers that don't support LSP
/// 3.17 pull diagnostics (`textDocument/diagnostic`) and fall back to
/// cached push diagnostics instead.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Checks whether `err` (as returned by `LspClient::request`) is a
/// server-side JSON-RPC error with the given code. `RpcError` itself stays
/// private to this module — `anyhow::Error` preserves the concrete source
/// type for downcasting, so this is a typed check rather than the fragile
/// `err.to_string().contains("-32601")` string-matching this replaced.
pub fn is_rpc_error_code(err: &anyhow::Error, code: i64) -> bool {
    matches!(err.downcast_ref::<RpcError>(), Some(RpcError::Server { code: c, .. }) if *c == code)
}

/// Typed request-level failure, so callers (namely the ContentModified retry
/// in `request()`) can match on the actual JSON-RPC error code instead of
/// substring-matching a formatted error string.
#[derive(Debug, Error)]
enum RpcError {
    #[error("LSP error {code}: {message}")]
    Server { code: i64, message: String },
    #[error("LSP server stdout closed while waiting for `{0}` response")]
    Closed(String),
    #[error("timed out waiting for `{0}` response")]
    Timeout(String),
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

/// Runs one LSP server process for the lifetime of a single CLI invocation:
/// spawn -> initialize -> one or more requests -> shutdown.
/// This mirrors the TS `LspClient` but does not persist across invocations
/// (this Rust port has no background manager daemon — see README for rationale).
pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    next_id: i64,
    incoming: mpsc::UnboundedReceiver<Value>,
    /// Most recent `textDocument/publishDiagnostics` payload per URI. Not
    /// every server supports LSP 3.17 pull diagnostics
    /// (`textDocument/diagnostic`) — typescript-language-server notably
    /// doesn't, it only ever pushes — so `diagnostics()` falls back to this
    /// cache when a pull request comes back "method not found". Populated
    /// opportunistically any time a notification is drained, whether or not
    /// anyone asked for diagnostics.
    diagnostics: std::collections::HashMap<String, Vec<Value>>,
    /// Version counter per open document URI. `None`/absent means not open.
    /// See `sync_document` for why this exists.
    open_docs: std::collections::HashMap<String, i64>,
}

impl LspClient {
    pub async fn spawn(server_path: &str, args: &[String], workspace_root: &str) -> Result<Self> {
        let mut child = tokio::process::Command::new(server_path)
            .args(args)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Belt-and-suspenders cleanup: without this, dropping an LspClient
            // on an error path (any `?` before shutdown() runs — see
            // commands.rs) leaves the child running unless it happens to
            // notice stdin EOF and self-exit, which isn't guaranteed by the
            // LSP spec. kill_on_drop makes tokio SIGKILL the child the moment
            // this value (or the process it's inside, via tokio's orphan
            // reaper) is dropped, regardless of why.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("failed to spawn LSP server `{server_path}`: {e}"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stdout, tx));

        Ok(Self {
            child,
            stdin,
            next_id: 1,
            incoming: rx,
            diagnostics: std::collections::HashMap::new(),
            open_docs: std::collections::HashMap::new(),
        })
    }

    /// OS process id of the spawned server, for diagnostics (`lsp server list`).
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Best-effort liveness check: `true` unless the child has already
    /// exited (or we failed to check, in which case we optimistically say
    /// it's still alive rather than false-positively reporting it dead).
    pub fn is_alive(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(Some(_)))
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.stdin.write_all(framed.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Send a request and wait for its matching response, draining any
    /// interleaved notifications in the meantime. Server-initiated requests
    /// (messages with both `id` and `method`, e.g. `workspace/configuration`,
    /// `client/registerCapability`, `workspace/diagnostic/refresh`) are
    /// answered with a minimal default response — some servers (observed
    /// with rust-analyzer) stall or misbehave if these are left unanswered.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        // Absolute wall-clock deadline for the whole call, independent of
        // wait_for_response's per-message 30s idle timeout (which resets on
        // every notification/server-request received, so a chatty-but-stuck
        // server could otherwise hang a command forever) and independent of
        // the ContentModified retry loop below.
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.request_inner(method, params),
        )
        .await
        .map_err(|_| anyhow!("`{method}` did not complete within 120s"))?
    }

    async fn request_inner(&mut self, method: &str, params: Value) -> Result<Value> {
        // LSP error -32801 (ContentModified) means the server was still
        // processing an earlier change (e.g. rust-analyzer mid-reindex) and
        // the spec requires the client to silently retry — it is not a real
        // failure. Retry with backoff instead of surfacing it to the caller.
        let mut attempt = 0;
        loop {
            let id = self.next_id;
            self.next_id += 1;
            let msg =
                json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params.clone() });
            self.send(&msg).await?;

            match self.wait_for_response(id, method).await {
                Ok(v) => return Ok(v),
                Err(RpcError::Server { code, .. }) if code == CONTENT_MODIFIED && attempt < 20 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    async fn wait_for_response(
        &mut self,
        id: i64,
        method: &str,
    ) -> std::result::Result<Value, RpcError> {
        loop {
            let msg = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.incoming.recv(),
            )
            .await
            {
                Ok(Some(m)) => m,
                Ok(None) => return Err(RpcError::Closed(method.to_string())),
                Err(_) => return Err(RpcError::Timeout(method.to_string())),
            };

            let msg_id = msg.get("id").and_then(|v| v.as_i64());
            let server_method = msg.get("method").and_then(|m| m.as_str());

            if let (Some(server_id), Some(server_method)) = (msg_id, server_method) {
                // Server-initiated request; answer it so the server doesn't stall.
                self.respond_to_server_request(server_id, server_method)
                    .await
                    .map_err(RpcError::Io)?;
                continue;
            }

            if let Some(resp_id) = msg_id {
                if resp_id == id {
                    if let Some(err) = msg.get("error") {
                        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                        let message = err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("")
                            .to_string();
                        return Err(RpcError::Server { code, message });
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                // else: stale response for an id we're no longer waiting on, ignore
                continue;
            }

            self.maybe_record_notification(server_method, &msg);
        }
    }

    /// Opportunistically caches `textDocument/publishDiagnostics` payloads
    /// as they're drained, whether or not the current caller asked for
    /// them — see the `diagnostics` field doc comment.
    fn maybe_record_notification(&mut self, method: Option<&str>, msg: &Value) {
        if method != Some("textDocument/publishDiagnostics") {
            return;
        }
        let Some(params) = msg.get("params") else {
            return;
        };
        let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
            return;
        };
        let items = params
            .get("diagnostics")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        let items = items.as_array().cloned().unwrap_or_default();
        self.diagnostics.insert(uri.to_string(), items);
    }

    /// Drains whatever's already buffered in the incoming channel (without
    /// blocking) so any `publishDiagnostics` notifications the server sent
    /// after a recent `didOpen` get captured into the cache before
    /// `cached_diagnostics` is read. The reader task keeps filling this
    /// channel in the background regardless of whether anyone's consuming
    /// it, so this is just catching up, not waiting on the server.
    pub async fn drain_pending_notifications(&mut self) {
        while let Ok(msg) = self.incoming.try_recv() {
            let msg_id = msg.get("id").and_then(|v| v.as_i64());
            let server_method = msg.get("method").and_then(|m| m.as_str());
            if let (Some(server_id), Some(server_method)) = (msg_id, server_method) {
                let _ = self
                    .respond_to_server_request(server_id, server_method)
                    .await;
                continue;
            }
            if msg_id.is_some() {
                continue; // stale response for a request nobody's awaiting anymore
            }
            self.maybe_record_notification(server_method, &msg);
        }
    }

    pub fn cached_diagnostics(&self, uri: &str) -> Vec<Value> {
        self.diagnostics.get(uri).cloned().unwrap_or_default()
    }

    async fn respond_to_server_request(&mut self, id: i64, method: &str) -> Result<()> {
        let result = match method {
            "workspace/configuration" => json!([]),
            _ => Value::Null,
        };
        let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        self.send(&msg).await
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&msg).await
    }

    /// Opens `uri` in this server, or — if it's already open in this warm
    /// session (from an earlier call reusing the same server) — sends a
    /// full-text `didChange` instead of a second `didOpen`.
    ///
    /// This matters beyond efficiency: every navigation command
    /// unconditionally "opens" its target file before querying, so with
    /// warm server reuse the *second* call against an already-open file
    /// used to send a duplicate `didOpen`. Observed live against
    /// typescript-language-server: it rejects that with `Can't open
    /// already open document` and silently skips reprocessing the file —
    /// which starved `textDocument/publishDiagnostics` of ever firing for
    /// that call, since the server never re-analyzed the "already open"
    /// document. `didChange` is what every real editor sends for a
    /// still-open document and is always accepted.
    pub async fn sync_document(&mut self, uri: &str, language_id: &str, text: &str) -> Result<()> {
        if let Some(version) = self.open_docs.get_mut(uri) {
            *version += 1;
            let version = *version;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }]
                }),
            )
            .await
        } else {
            self.open_docs.insert(uri.to_string(), 1);
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
            )
            .await
        }
    }

    pub async fn initialize(&mut self, workspace_root: &str) -> Result<Value> {
        let uri = lsp::uri::from_path(std::path::Path::new(workspace_root));
        let name = std::path::Path::new(workspace_root)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let result = self
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": uri,
                    "capabilities": {
                        "textDocument": {
                            "synchronization": {"didOpen": true, "didClose": true},
                            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                            "definition": {"linkSupport": true},
                            "references": {},
                            "hover": {},
                            "implementation": {"linkSupport": true},
                            "typeDefinition": {"linkSupport": true},
                            "declaration": {"linkSupport": true},
                            "diagnostic": {},
                            "publishDiagnostics": {},
                            "callHierarchy": {}
                        },
                        "workspace": {"symbol": {}}
                    },
                    "workspaceFolders": [{"uri": uri, "name": name}]
                }),
            )
            .await?;
        self.notify("initialized", json!({})).await?;
        Ok(result)
    }

    pub async fn shutdown(&mut self) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.request("shutdown", Value::Null),
        )
        .await;
        let _ = self.notify("exit", Value::Null).await;
        let _ = self.child.start_kill();
    }
}

/// Read chunk size. Large enough that a typical response arrives in one or
/// two reads; the framing below handles arbitrary splits either way.
const READ_CHUNK_BYTES: usize = 8192;

/// Length of the `\r\n\r\n` sequence terminating an LSP header block.
const HEADER_TERMINATOR_LEN: usize = 4;

async fn read_loop(stdout: tokio::process::ChildStdout, tx: mpsc::UnboundedSender<Value>) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }

        for msg in drain_messages(&mut buf) {
            let _ = tx.send(msg);
        }
    }
}

/// Pulls every complete message out of `buf`, consuming exactly the bytes
/// it parsed and leaving any partial trailing message in place.
///
/// Split out of `read_loop` so the framing can be tested directly — the
/// loop itself needs a real `ChildStdout`, so the framing tests used to
/// re-implement this parser by hand and therefore couldn't catch a bug in
/// it.
fn drain_messages(buf: &mut Vec<u8>) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let Some(header_end) = find_header_end(buf) else {
            return out;
        };
        let header_str = String::from_utf8_lossy(&buf[..header_end]);
        let len = header_str
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
            })
            .and_then(|v| v.parse::<usize>().ok());
        let Some(len) = len else {
            // A header block with no parseable Content-Length is not an
            // LSP message — a server that wrote a banner or a log line to
            // stdout, say. Discard it and resynchronize.
            //
            // This used to return without consuming anything, which was
            // unrecoverable: the caller appended more bytes,
            // `find_header_end` re-found the *same* terminator (it always
            // returns the first one), and gave up again. From the first
            // stray byte onward no message was ever parsed again, every
            // request ran out its 30s idle and 120s wall-clock timeouts,
            // and `buf` grew for the life of the process.
            buf.drain(..header_end + HEADER_TERMINATOR_LEN);
            continue;
        };
        let body_start = header_end + HEADER_TERMINATOR_LEN;
        if buf.len() < body_start + len {
            return out; // body still in flight; keep the partial frame
        }
        if let Ok(v) = serde_json::from_slice::<Value>(&buf[body_start..body_start + len]) {
            out.push(v);
        }
        buf.drain(..body_start + len);
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_header_terminator() {
        let buf = b"Content-Length: 5\r\n\r\nhello";
        assert_eq!(find_header_end(buf), Some(17));
    }

    #[test]
    fn no_header_terminator_returns_none() {
        let buf = b"Content-Length: 5\r\nhello";
        assert_eq!(find_header_end(buf), None);
    }

    /// Frames a JSON value the way a server would put it on the wire.
    fn framed(v: Value) -> String {
        let body = v.to_string();
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
    }

    // These exercise `drain_messages`, the real parser. They previously
    // re-implemented header parsing inline and asserted against their own
    // reimplementation, so a bug in the production path passed unnoticed —
    // including the resynchronization bug the last test here covers.

    #[test]
    fn parses_a_single_message_and_consumes_exactly_its_bytes() {
        let mut buf =
            framed(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
                .into_bytes();
        let msgs = drain_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["method"], "initialize");
        assert!(buf.is_empty(), "parsed bytes should be consumed");
    }

    #[test]
    fn parses_two_back_to_back_messages() {
        let mut buf = format!(
            "{}{}",
            framed(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": null})),
            framed(serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": null}))
        )
        .into_bytes();
        let msgs = drain_messages(&mut buf);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["id"], 1);
        assert_eq!(msgs[1]["id"], 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_partial_message_is_left_in_the_buffer_until_the_rest_arrives() {
        let whole = framed(serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": 42}));
        let split_at = whole.len() - 5;
        let mut buf = whole.as_bytes()[..split_at].to_vec();

        assert!(drain_messages(&mut buf).is_empty(), "body not complete yet");
        buf.extend_from_slice(&whole.as_bytes()[split_at..]);

        let msgs = drain_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["id"], 7);
        assert!(buf.is_empty());
    }

    #[test]
    fn resynchronizes_after_a_header_block_with_no_content_length() {
        // A server that writes a non-LSP banner to stdout (jdtls and
        // gradle-backed servers do) used to desync the reader permanently:
        // the parser gave up without consuming the bad block, then re-found
        // the same terminator on every subsequent read and gave up again,
        // so no message was ever parsed for the life of the process.
        let good = framed(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": "ok"}));
        let mut buf = format!("Some-Header: banner\r\n\r\n{good}").into_bytes();

        let msgs = drain_messages(&mut buf);
        assert_eq!(msgs.len(), 1, "should skip the junk block and recover");
        assert_eq!(msgs[0]["result"], "ok");
        assert!(buf.is_empty());
    }

    #[test]
    fn a_body_that_is_not_valid_json_is_dropped_without_desyncing() {
        let good = framed(serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": "after"}));
        let mut buf = format!("Content-Length: 3\r\n\r\nnot{good}").into_bytes();

        let msgs = drain_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["result"], "after");
        assert!(buf.is_empty());
    }

    #[test]
    fn header_name_matching_is_case_insensitive() {
        let body = serde_json::json!({"id": 1}).to_string();
        let mut buf = format!("content-length: {}\r\n\r\n{}", body.len(), body).into_bytes();
        assert_eq!(drain_messages(&mut buf).len(), 1);
    }

    #[test]
    fn extra_headers_alongside_content_length_are_tolerated() {
        let body = serde_json::json!({"id": 9}).to_string();
        let mut buf = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
        let msgs = drain_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["id"], 9);
    }
}
