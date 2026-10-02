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

/// Owns one LSP server process: spawn -> initialize -> requests -> shutdown.
///
/// Instances live in the background daemon (`src/daemon.rs`), not in the
/// CLI process, so a server started for a project stays warm and is reused
/// across separate CLI invocations until it is stopped, idles out, or is
/// found dead. The comment that used to sit here claimed the opposite —
/// that this port had no daemon — which predates `daemon.rs` entirely and
/// gave anyone reading this module first exactly the wrong model.
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
    /// Every document this server has open, with the exact text it was
    /// given. See `sync_document` and `resync_from_disk` for why the text
    /// (and the file's on-disk stamp) is kept.
    open_docs: std::collections::HashMap<String, OpenDoc>,
    /// Bumped on every didOpen/didChange/didClose this client sends, so a
    /// diagnostics consumer can tell whether a cached `publishDiagnostics`
    /// predates the latest edit. See `diagnostics_are_current`.
    sync_generation: u64,
    /// `sync_generation` as of the last `publishDiagnostics` per URI.
    diagnostics_generation: std::collections::HashMap<String, u64>,
    /// How many `publishDiagnostics` have arrived per URI, so a waiter can
    /// tell when a server has stopped publishing (see `publish_count`).
    diagnostics_publishes: std::collections::HashMap<String, u64>,
    /// `sync_generation` as of the last time a caller already waited for
    /// diagnostics on a URI. typescript-language-server only publishes when
    /// a file's diagnostics *change*, so after an unrelated edit no publish
    /// may ever come; without remembering the wait, every later call paid
    /// it again.
    diagnostics_waited: std::collections::HashMap<String, u64>,
    /// Monotonic use counter for least-recently-used eviction.
    use_clock: u64,
    /// See `StderrTail`.
    stderr_tail: StderrTail,
    /// Whether this client's death has been logged already.
    death_reported: bool,
    /// Work-done progress the server has begun and not yet ended (tokens
    /// rendered as strings). Servers report project loading this way —
    /// typescript-language-server's "Initializing JS/TS language features",
    /// gopls's "Loading packages", rust-analyzer's indexing — and answer
    /// navigation requests incompletely until it ends. See `is_busy`.
    active_progress: std::collections::HashSet<String>,
    /// Whether any progress has been reported at all, so a readiness wait
    /// knows whether this server reports progress.
    saw_progress: bool,
    /// rust-analyzer's `experimental/serverStatus` `quiescent` flag.
    quiescent: Option<bool>,
}

/// The last lines a server wrote to stderr: kept so a crash can be
/// explained, bounded so a chatty server can't grow it. Reading the pipe
/// continuously also keeps a server that logs a lot from blocking on a
/// full stderr pipe.
#[derive(Clone, Default)]
struct StderrTail {
    lines: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Set once the pipe hit end-of-file: everything the server wrote has
    /// been collected.
    closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

const STDERR_TAIL_LINES: usize = 40;
const STDERR_LINE_MAX: usize = 400;

impl StderrTail {
    async fn collect(self, stderr: tokio::process::ChildStderr) {
        use tokio::io::AsyncBufReadExt;
        // Bytes, decoded lossily: a server may log a Latin-1 path or raw
        // file content, and `lines()` gives up on the first line that isn't
        // UTF-8 — after which nobody reads the pipe and the server's own
        // writes to stderr start failing.
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    self.push(line.trim_end_matches(['\n', '\r']).to_string());
                }
                Err(_) => break,
            }
        }
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn push(&self, mut line: String) {
        if line.len() > STDERR_LINE_MAX {
            let mut cut = STDERR_LINE_MAX;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push('…');
        }
        let mut tail = self.lines.lock().unwrap();
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    fn text(&self) -> String {
        let tail = self.lines.lock().unwrap();
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// A document open in the server.
struct OpenDoc {
    version: i64,
    text: String,
    /// `(mtime, length)` of the file when `text` was last read from it, so
    /// `resync_from_disk` can skip unchanged files with a single `stat`.
    stamp: Option<(std::time::SystemTime, u64)>,
    last_used: u64,
}

/// How many documents one server keeps open at once.
///
/// Every document a command touches used to stay open for the server's
/// whole life. Besides the memory, each open document is one more file
/// whose in-memory copy overrides the disk and has to be kept in sync, so
/// least-recently-used ones beyond this are closed — after which the server
/// reads them from disk like any other file in the project.
const MAX_OPEN_DOCS: usize = 16;

/// `(mtime, length)` of `path`, if it can be trusted to identify the
/// file's content.
///
/// A file modified within the last couple of seconds gets `None`: on a
/// filesystem with coarse timestamps, two same-length writes inside one
/// tick share a stamp, and a write landing between the CLI reading a file
/// and the daemon stamping it would pair the new stamp with the old text.
/// Either way the edit would be skipped forever. With no stamp, the next
/// resync compares content instead, which costs one read of a recently
/// edited file.
fn disk_stamp(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    (age >= STAMP_TRUSTED_AFTER).then_some((modified, meta.len()))
}

/// See `disk_stamp`.
const STAMP_TRUSTED_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

impl LspClient {
    pub async fn spawn(server_path: &str, args: &[String], workspace_root: &str) -> Result<Self> {
        let mut child = tokio::process::Command::new(server_path)
            .args(args)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
        let stderr_tail = StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(stderr_tail.clone().collect(stderr));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stdout, tx));

        Ok(Self {
            child,
            stdin,
            next_id: 1,
            incoming: rx,
            diagnostics: std::collections::HashMap::new(),
            open_docs: std::collections::HashMap::new(),
            stderr_tail,
            death_reported: false,
            sync_generation: 0,
            diagnostics_generation: std::collections::HashMap::new(),
            diagnostics_publishes: std::collections::HashMap::new(),
            diagnostics_waited: std::collections::HashMap::new(),
            use_clock: 0,
            active_progress: std::collections::HashSet::new(),
            saw_progress: false,
            quiescent: None,
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
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.request_inner(method, params),
        )
        .await
        .map_err(|_| anyhow!("`{method}` did not complete within 120s"))?;
        match outcome {
            Ok(v) => Ok(v),
            Err(e) => Err(self.with_stderr_if_dead(e).await),
        }
    }

    /// If the server has died, attaches the last of what it printed to
    /// stderr to `err`. Server stderr used to go to /dev/null, so a crash
    /// surfaced as a bare "stdout closed" with no hint of why.
    pub async fn with_stderr_if_dead(&mut self, err: anyhow::Error) -> anyhow::Error {
        // Its stdout closing is the server going away, but the exit itself
        // can become observable a moment later; checking liveness at once
        // raced it and reported a crash as a bare "stdout closed".
        if matches!(err.downcast_ref::<RpcError>(), Some(RpcError::Closed(_))) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
            while self.is_alive() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        if self.is_alive() {
            return err;
        }
        if !self.death_reported {
            // Let the reader task collect the final lines (it sees EOF once
            // the process is gone; a fixed pause lost them under load), and
            // log them once, not on every later request to this dead client
            // before it is reaped.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
            while !self
                .stderr_tail
                .closed
                .load(std::sync::atomic::Ordering::Acquire)
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            self.death_reported = true;
            let tail = self.stderr_tail.text();
            if !tail.is_empty() {
                eprintln!("[daemon] language server exited; its last output:\n{tail}");
            }
        }
        let tail = self.stderr_tail.text();
        if tail.is_empty() {
            anyhow!("{err} (the language server exited)")
        } else {
            anyhow!("{err}\nThe language server exited. Its last output:\n{tail}")
        }
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

            if is_server_request(&msg) {
                // Server-initiated request; answer it so the server doesn't stall.
                self.respond_to_server_request(&msg)
                    .await
                    .map_err(RpcError::Io)?;
                continue;
            }

            let msg_id = msg.get("id").and_then(|v| v.as_i64());
            let server_method = msg.get("method").and_then(|m| m.as_str());
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
        match method {
            Some("$/progress") => {
                self.record_progress(msg);
                return;
            }
            Some("experimental/serverStatus") => {
                self.quiescent = msg
                    .get("params")
                    .and_then(|p| p.get("quiescent"))
                    .and_then(|q| q.as_bool());
                return;
            }
            Some("textDocument/publishDiagnostics") => {}
            _ => return,
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
        self.diagnostics_generation
            .insert(uri.to_string(), self.sync_generation);
        *self
            .diagnostics_publishes
            .entry(uri.to_string())
            .or_default() += 1;
    }

    /// Drains whatever's already buffered in the incoming channel (without
    /// blocking) so any `publishDiagnostics` notifications the server sent
    /// after a recent `didOpen` get captured into the cache before
    /// `cached_diagnostics` is read. The reader task keeps filling this
    /// channel in the background regardless of whether anyone's consuming
    /// it, so this is just catching up, not waiting on the server.
    pub async fn drain_pending_notifications(&mut self) {
        while let Ok(msg) = self.incoming.try_recv() {
            if is_server_request(&msg) {
                let _ = self.respond_to_server_request(&msg).await;
                continue;
            }
            let server_method = msg.get("method").and_then(|m| m.as_str());
            if msg.get("id").is_some() {
                continue; // stale response for a request nobody's awaiting anymore
            }
            self.maybe_record_notification(server_method, &msg);
        }
    }

    fn record_progress(&mut self, msg: &Value) {
        let Some(params) = msg.get("params") else {
            return;
        };
        let token = match params.get("token") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => return,
        };
        match params
            .get("value")
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str())
        {
            Some("begin") => {
                self.saw_progress = true;
                self.active_progress.insert(token);
            }
            Some("end") => {
                self.active_progress.remove(&token);
            }
            _ => {}
        }
    }

    /// Whether the server says it is still loading or indexing: some
    /// work-done progress is open, or rust-analyzer reports itself not
    /// quiescent.
    pub fn is_busy(&self) -> bool {
        !self.active_progress.is_empty() || self.quiescent == Some(false)
    }

    /// Whether this server has reported any progress or status yet.
    pub fn reports_progress(&self) -> bool {
        self.saw_progress || self.quiescent.is_some()
    }

    pub fn cached_diagnostics(&self, uri: &str) -> Vec<Value> {
        self.diagnostics.get(uri).cloned().unwrap_or_default()
    }

    async fn respond_to_server_request(&mut self, request: &Value) -> Result<()> {
        self.send(&server_request_response(request)).await
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&msg).await
    }

    /// Opens `uri` in this server with `text`, or — if it's already open
    /// in this warm session — updates it with a full-text `didChange`.
    /// Returns whether the server's view of the document changed.
    ///
    /// Re-opening an already-open document used to send a duplicate
    /// `didOpen`. typescript-language-server rejects that (`Can't open
    /// already open document`) and silently skips reprocessing, which
    /// starved diagnostics of ever re-running for that call. `didChange` is
    /// what every editor sends for a still-open document.
    ///
    /// Identical text sends nothing at all: there is nothing for the server
    /// to re-analyze, and the caller can skip waiting for it to.
    pub async fn sync_document(
        &mut self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<bool> {
        self.use_clock += 1;
        let now = self.use_clock;
        let stamp = disk_stamp(&lsp::uri::to_path(uri));
        let changed = if let Some(doc) = self.open_docs.get_mut(uri) {
            doc.last_used = now;
            doc.stamp = stamp;
            if doc.text == text {
                false
            } else {
                let version = doc.version + 1;
                // Recorded only once sent: a write that fails (or is cut
                // off) must not leave this client believing the server has
                // text it never received, which no later resync would
                // correct.
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
                if let Some(doc) = self.open_docs.get_mut(uri) {
                    doc.version = version;
                    doc.text = text.to_string();
                }
                true
            }
        } else {
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
            )
            .await?;
            self.open_docs.insert(
                uri.to_string(),
                OpenDoc {
                    version: 1,
                    text: text.to_string(),
                    stamp,
                    last_used: now,
                },
            );
            true
        };
        if changed {
            self.sync_generation += 1;
        }
        self.close_least_recently_used(uri).await?;
        Ok(changed)
    }

    /// Brings every open document other than `except` back in line with
    /// the disk. Returns whether anything was sent.
    ///
    /// For an open document the server's copy overrides the file, and
    /// `didChangeWatchedFiles` doesn't change that. So once a command had
    /// opened a file, an edit to it on disk was invisible to every later
    /// query that targeted a *different* file: definitions pointed at old
    /// lines, diagnostics missed new errors, and a rename computed its
    /// edits against the stale text and `--apply` wrote them at the wrong
    /// offsets. A `stat` per open document (at most `MAX_OPEN_DOCS`) is
    /// the whole cost when nothing changed.
    pub async fn resync_from_disk(&mut self, except: Option<&str>) -> Result<bool> {
        let mut changed = false;
        let uris: Vec<String> = self
            .open_docs
            .keys()
            .filter(|u| Some(u.as_str()) != except)
            .cloned()
            .collect();
        for uri in uris {
            let path = lsp::uri::to_path(&uri);
            let stamp = disk_stamp(&path);
            let Some(doc) = self.open_docs.get_mut(&uri) else {
                continue;
            };
            if stamp.is_some() && stamp == doc.stamp {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    if text == doc.text {
                        doc.stamp = stamp;
                        continue;
                    }
                    // Sent first, recorded after (see `sync_document`).
                    let version = doc.version + 1;
                    let msg = json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    });
                    self.notify("textDocument/didChange", msg).await?;
                    if let Some(doc) = self.open_docs.get_mut(&uri) {
                        doc.stamp = stamp;
                        doc.version = version;
                        doc.text = text;
                    }
                }
                // Deleted (or unreadable): stop overriding it, so the server
                // goes back to whatever the disk says.
                Err(_) => self.close_document(&uri).await?,
            }
            changed = true;
        }
        if changed {
            self.sync_generation += 1;
        }
        Ok(changed)
    }

    async fn close_document(&mut self, uri: &str) -> Result<()> {
        if self.open_docs.remove(uri).is_some() {
            self.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await?;
            self.sync_generation += 1;
        }
        Ok(())
    }

    async fn close_least_recently_used(&mut self, keep: &str) -> Result<()> {
        while self.open_docs.len() > MAX_OPEN_DOCS {
            let Some(oldest) = self
                .open_docs
                .iter()
                .filter(|(u, _)| u.as_str() != keep)
                .min_by_key(|(_, d)| d.last_used)
                .map(|(u, _)| u.clone())
            else {
                break;
            };
            self.close_document(&oldest).await?;
        }
        Ok(())
    }

    /// Has the server re-read `path` from disk, if it isn't one of our open
    /// documents (those are kept current by `resync_from_disk`): a
    /// `didOpen` with the current content, then a `didClose`, after which
    /// the server goes back to the file on disk. See
    /// `daemon::reload_changed_files`.
    pub async fn reload_from_disk(
        &mut self,
        path: &std::path::Path,
        language_id: &str,
    ) -> Result<()> {
        let uri = lsp::uri::from_path(path);
        if self.open_docs.contains_key(&uri) {
            return Ok(());
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(());
        };
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text } }),
        )
        .await?;
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await?;
        self.sync_generation += 1;
        Ok(())
    }

    /// Records that files changed without this client sending the change
    /// as a document edit (`workspace/didChangeWatchedFiles`): published
    /// diagnostics may be stale until the server publishes again.
    pub fn mark_external_change(&mut self) {
        self.sync_generation += 1;
    }

    /// Number of `publishDiagnostics` received for `uri` so far.
    pub fn publish_count(&self, uri: &str) -> u64 {
        self.diagnostics_publishes.get(uri).copied().unwrap_or(0)
    }

    /// Whether the cached `publishDiagnostics` for `uri` arrived after the
    /// most recent document change this client sent.
    ///
    /// Also true once a caller has already waited out a change for this URI
    /// (`mark_diagnostics_waited`) — a server that had nothing new to say
    /// then isn't going to say it on the next call either.
    pub fn diagnostics_are_current(&self, uri: &str) -> bool {
        let current = |m: &std::collections::HashMap<String, u64>| {
            m.get(uri).is_some_and(|g| *g >= self.sync_generation)
        };
        current(&self.diagnostics_generation) || current(&self.diagnostics_waited)
    }

    /// Records that the cached diagnostics for `uri` have been waited for
    /// as of the current document state.
    pub fn mark_diagnostics_waited(&mut self, uri: &str) {
        self.diagnostics_waited
            .insert(uri.to_string(), self.sync_generation);
    }

    /// Processes incoming messages for up to `timeout` (answering server
    /// requests and caching notifications), returning early once `done`
    /// holds. For waits on push-only state such as diagnostics.
    pub async fn pump_until(&mut self, timeout: std::time::Duration, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.drain_pending_notifications().await;
            if done(self) {
                return;
            }
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, self.incoming.recv()).await
            else {
                return;
            };
            if is_server_request(&msg) {
                let _ = self.respond_to_server_request(&msg).await;
                continue;
            }
            let method = msg
                .get("method")
                .and_then(|m| m.as_str())
                .map(str::to_string);
            if msg.get("id").is_none() {
                self.maybe_record_notification(method.as_deref(), &msg);
            }
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
                            "callHierarchy": {},
                            // `hierarchy` and `rename` are shipped
                            // commands, but neither capability was
                            // announced. A server that registers providers
                            // based on what the client claims to support
                            // would refuse both — indistinguishable, from
                            // the outside, from "this server doesn't
                            // implement them".
                            "typeHierarchy": {},
                            "rename": {"prepareSupport": false}
                        },
                        // Without these, servers have no channel to say
                        // "still loading the project", and answer requests
                        // made in the meantime incompletely (a definition
                        // that stops at the import). See `is_busy`.
                        "window": {"workDoneProgress": true},
                        "experimental": {"serverStatusNotification": true},
                        "workspace": {
                            "symbol": {},
                            // `collect_edits` handles the `documentChanges`
                            // form of a WorkspaceEdit, so say so; without
                            // it a server is entitled to reply with only
                            // the older flat `changes` map.
                            "workspaceEdit": {"documentChanges": true}
                        }
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

/// Whether `msg` is a request *from* the server (it has both a `method` and
/// an `id`). JSON-RPC ids may be numbers or strings; only numbers used to
/// be recognized, so a server using string ids had its requests mistaken
/// for notifications, never answered, and could stall waiting on them.
fn is_server_request(msg: &Value) -> bool {
    msg.get("method").and_then(|m| m.as_str()).is_some()
        && msg
            .get("id")
            .is_some_and(|id| id.is_number() || id.is_string())
}

/// The minimal reply to a server-initiated request, echoing its id as-is.
///
/// `workspace/configuration` must answer with one entry per requested
/// item, in order (LSP 3.17); `null` means "no configuration, use your
/// defaults". It used to answer `[]` regardless, which a server indexing
/// the response by item position reads as malformed.
fn server_request_response(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let result = match request.get("method").and_then(|m| m.as_str()) {
        Some("workspace/configuration") => {
            let items = request
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(|i| i.as_array())
                .map_or(0, |a| a.len());
            Value::Array(vec![Value::Null; items])
        }
        _ => Value::Null,
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

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
    fn the_stderr_tail_keeps_only_the_last_lines_and_caps_their_length() {
        let tail = StderrTail::default();
        for i in 0..100 {
            tail.push(format!("line {i}"));
        }
        let text = tail.text();
        assert_eq!(text.lines().count(), STDERR_TAIL_LINES);
        assert!(
            text.starts_with("line 60\n") && text.ends_with("line 99"),
            "{text}"
        );
        tail.push("é".repeat(1000));
        let last = tail.text().lines().last().unwrap().to_string();
        assert!(
            last.len() <= STDERR_LINE_MAX + '…'.len_utf8(),
            "{}",
            last.len()
        );
        assert!(last.ends_with('…'));
    }

    #[test]
    fn server_requests_are_recognized_with_string_or_numeric_ids() {
        assert!(is_server_request(
            &json!({"jsonrpc":"2.0","id":"abc","method":"window/workDoneProgress/create","params":{}})
        ));
        assert!(is_server_request(
            &json!({"jsonrpc":"2.0","id":7,"method":"client/registerCapability"})
        ));
        // A notification has no id; a response has no method.
        assert!(!is_server_request(
            &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}})
        ));
        assert!(!is_server_request(
            &json!({"jsonrpc":"2.0","id":3,"result":null})
        ));
    }

    #[test]
    fn server_request_response_echoes_a_string_id_verbatim() {
        let reply = server_request_response(
            &json!({"jsonrpc":"2.0","id":"req-1","method":"client/registerCapability"}),
        );
        assert_eq!(reply, json!({"jsonrpc":"2.0","id":"req-1","result":null}));
    }

    #[test]
    fn workspace_configuration_gets_one_null_per_requested_item() {
        let reply = server_request_response(&json!({
            "jsonrpc":"2.0","id":4,"method":"workspace/configuration",
            "params":{"items":[{"section":"a"},{"section":"b"},{"scopeUri":"file:///x"}]}
        }));
        assert_eq!(reply["id"], json!(4));
        assert_eq!(reply["result"], json!([null, null, null]));
        let reply = server_request_response(
            &json!({"jsonrpc":"2.0","id":5,"method":"workspace/configuration"}),
        );
        assert_eq!(reply["result"], json!([]));
    }

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
