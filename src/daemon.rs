//! Background manager daemon: tracks long-lived LSP server processes over a
//! Unix Domain Socket, mirroring manager/daemon.ts. Started on demand by
//! `lsp server start/list/stop` and by the navigation commands via
//! `lsp --daemon` (see manager_client.rs).
//!
//! The navigation commands (outline, definition, reference, doc, symbol,
//! search) proxy through this daemon via `/request` and `/notify` — see
//! `proxy_request`/`proxy_notify` below and `commands.rs`'s
//! `ensure_daemon_session` — so a language server started for a project
//! stays warm and is reused across CLI invocations (different OS processes)
//! instead of being spawned and killed fresh on every single command. It's
//! evicted only on `lsp server stop`, an idle timeout (`idleTimeout` in
//! `~/.lsp-cli/config.json`, default 600s / 10 minutes), or if it's found to
//! have died (see `Manager::create`'s liveness check).

use crate::lsp_client::LspClient;
use crate::registry::{default_install_dir, server_path};
use crate::watcher::WatcherManager;
use anyhow::Result;
use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedServerInfo {
    pub project_root: String,
    pub language: String,
    pub status: String, // starting | running | stopped
    pub idle_since: i64,
    /// OS process id of the spawned language server, for diagnostics and so
    /// callers/tests can verify a "reload" actually replaced the process.
    pub pid: Option<u32>,
    /// True when this `create` call spawned the process, false when it
    /// handed back an already-warm one.
    ///
    /// Lets the CLI size its post-`didOpen` settle wait to the situation
    /// instead of paying one worst-case constant every time: a server that
    /// has just finished `initialize` still has a whole project to index
    /// (gopls answers `no package metadata` until it has), while a warm one
    /// only needs to digest a single changed document. Not part of the
    /// server's persistent state — it describes this response, so it is
    /// excluded from `list` output.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub just_started: bool,
    /// Whether the server still reports itself busy loading the project
    /// (open work-done progress) as of this response. Like `just_started`,
    /// tells the CLI an incomplete-looking answer may be worth retrying.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub loading: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub path: String,
    /// Project root the caller has already resolved, when it differs from
    /// what the daemon would detect on its own — i.e. when the user passed
    /// `--project`. Without this the daemon re-derived the root from
    /// `path` and keyed the server on *that*, while every follow-up
    /// `/request` and `/notify` looked the server up by the caller's root.
    /// The two never matched, so `--project` failed every navigation
    /// command with "No server running for project: <root>".
    #[serde(default)]
    pub project_root: Option<String>,
    /// The server binary the CLI already resolved and validated (see
    /// `install::ensure_installed`). The daemon's own environment can
    /// differ from the CLI's — it may have been started from a GUI editor
    /// with a minimal `PATH` — so re-resolving here could pick a different
    /// binary, or none. Absent for bundled servers and deno, and from
    /// callers that didn't check (`server start`, search warm-up), where
    /// the daemon falls back to `registry::server_path`.
    #[serde(default)]
    pub server_path: Option<String>,
    /// The language the CLI resolved for `path`. See `Manager::create`.
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteRequest {
    pub path: Option<String>,
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub project_root: String,
    pub query: String,
}

#[derive(Debug, Deserialize)]
pub struct ProxyRequest {
    pub project_root: String,
    #[serde(default)]
    pub language: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

struct ManagedServer {
    client: Arc<Mutex<LspClient>>,
    info: ManagedServerInfo,
}

pub struct Manager {
    servers: Mutex<HashMap<String, ManagedServer>>,
    /// Per-project-root+language locks, serializing create()'s
    /// check-then-act sequence (including the slow spawn+initialize work)
    /// for a *given* key only — closing the race that otherwise lets
    /// concurrent `create()` calls for the *same* project each spawn their
    /// own LSP server process before either one gets far enough to insert
    /// into `servers`, silently orphaning the loser. Keyed rather than a
    /// single global lock so starting a server for project A doesn't block
    /// starting one for unrelated project B — `initialize` handshakes can
    /// take several seconds (longer under load/cold-start), and with a
    /// single lock that meant e.g. two agents working in different repos
    /// would serialize on each other's cold-starts for no reason. Entries
    /// are never removed (bounded by the number of distinct project+language
    /// keys ever created in this daemon's lifetime — negligible compared to
    /// the warm-server churn `reap_idle` already handles). Does not block
    /// `list`/`delete`, which only touch `servers` and stay fully concurrent.
    create_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Watches each project root with a live server for on-disk changes to
    /// files outside the one currently being queried, forwarding debounced
    /// `workspace/didChangeWatchedFiles` batches back to `broadcast_notify`
    /// via the channel returned alongside `Manager::new` (see
    /// `start_daemon`) — mirrors manager/watcher.ts. Only matters for
    /// `lsp server start`-warmed servers; the per-invocation navigation
    /// commands always read the current file fresh off disk and never see
    /// unwatched external edits in the first place.
    watcher: WatcherManager,
    /// BM25 fallback indexes, keyed by project root.
    ///
    /// Building one walks and reads every source file in the project and
    /// runs the per-language regex heuristics over each line, which is by
    /// far the most expensive thing this tool does. It used to happen in
    /// the CLI process on every single `lsp search` that fell back, and be
    /// dropped on exit — so two identical searches a second apart each paid
    /// for it in full.
    search_indexes: Mutex<HashMap<String, CachedIndex>>,
}

struct CachedIndex {
    index: Arc<crate::bm25::Bm25Index>,
    /// What the tree looked like when `index` was built. Re-checked on
    /// every search; see `TreeFingerprint` for why this rather than the
    /// file watcher.
    fingerprint: crate::bm25::TreeFingerprint,
    /// Last use, so the idle reaper can drop indexes for projects that
    /// have gone quiet instead of holding them for the daemon's lifetime.
    used_at: i64,
}

impl Manager {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<crate::watcher::WatchBatch>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                servers: Mutex::new(HashMap::new()),
                create_locks: Mutex::new(HashMap::new()),
                watcher: WatcherManager::new(tx),
                search_indexes: Mutex::new(HashMap::new()),
            },
            rx,
        )
    }

    /// Returns the per-key lock for `key`, creating it if this is the first
    /// time this key has been seen. Only the map lookup/insert is guarded
    /// by `create_locks` itself — the returned `Arc<Mutex<()>>` is what
    /// actually serializes `create()` for this one key.
    async fn create_lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        self.create_locks
            .lock()
            .await
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn list(&self) -> Vec<ManagedServerInfo> {
        self.servers
            .lock()
            .await
            .values()
            .map(|s| s.info.clone())
            .collect()
    }

    pub async fn create(
        &self,
        path: &str,
        project_root_override: Option<&str>,
        server_path_override: Option<&str>,
        language_override: Option<&str>,
    ) -> Result<ManagedServerInfo> {
        let file_path = std::path::Path::new(path);
        // The CLI has usually resolved the language already (it needs it
        // to install a server), and its resolution allows a file with no
        // root marker when `--project` names the root. Re-detecting here
        // from markers alone failed exactly that case with "Cannot detect
        // language for path", making `--project` unusable for it.
        let language_override = language_override.and_then(|name| {
            crate::registry::languages()
                .iter()
                .find(|l| l.name == name)
                .copied()
        });
        let detected = match (
            crate::registry::detect_for_path(file_path),
            language_override,
        ) {
            (Some(d), None) => d,
            (Some(d), Some(lang)) if d.lang.name == lang.name => d,
            // Marker detection found nothing (or disagrees): the caller's
            // language stands, and the root is the caller's override when
            // given (below), otherwise the file's directory.
            (_, Some(lang)) => crate::registry::Detected {
                lang,
                root: file_path.parent().unwrap_or(file_path).to_path_buf(),
            },
            (None, None) => anyhow::bail!("Cannot detect language for path: {path}"),
        };

        // The caller's root wins when it supplied one, so the key this
        // server is registered under is the same string later `/request`
        // and `/notify` calls will look it up by. The *language* still
        // comes from detection — an override says where the project is,
        // not what it's written in.
        let root = match project_root_override {
            Some(r) => r.to_string(),
            None => detected.root.to_string_lossy().to_string(),
        };
        let key = format!("{root}::{}", detected.lang.name);

        // Held for the whole spawn+initialize sequence below — see the
        // `create_locks` doc comment for why this is per-key, not global.
        let key_lock = self.create_lock_for(&key).await;
        let _create_guard = key_lock.lock().await;

        // Re-check (and, crucially, re-check *liveness*, not just presence)
        // now that we hold the lock. This is what makes "kill and reload"
        // actually work: a cached entry whose underlying process has died
        // (crashed, OOM-killed, `kill -9`'d externally) is detected here and
        // evicted instead of being handed back as if it were still good.
        //
        // The `servers` map lock is *not* held while checking liveness.
        // Checking needs the client's own lock, which an in-flight request
        // holds for up to the 120s request ceiling; holding the map lock
        // across that wait stalled every `/list`, `/request` and `/notify`
        // for every project in the daemon — including the CLI's own
        // `is_alive()` probe, which is a `/list`. A client that is busy
        // answering a request is, for this purpose, alive: if it dies
        // mid-request that request fails and the next `create` evicts it.
        let existing = self
            .servers
            .lock()
            .await
            .get(&key)
            .map(|e| e.client.clone());
        if let Some(client) = existing {
            let (alive, loading) = match client.try_lock() {
                Ok(mut c) => {
                    c.drain_pending_notifications().await;
                    (c.is_alive(), c.is_busy())
                }
                Err(_) => (true, false),
            };
            let mut servers = self.servers.lock().await;
            // Only act on the entry we inspected: the reaper may have
            // replaced or removed it in between (we hold the per-key create
            // lock, so nothing else can have *inserted* one).
            let same = servers
                .get(&key)
                .is_some_and(|e| Arc::ptr_eq(&e.client, &client));
            if same && alive {
                let entry = servers.get_mut(&key).expect("checked above");
                entry.info.idle_since = now_ms();
                let mut info = entry.info.clone();
                info.just_started = false;
                info.loading = loading;
                return Ok(info);
            }
            if same {
                servers.remove(&key);
            }
        }

        let install_dir = default_install_dir();
        let bin = match server_path_override {
            Some(p) => std::path::PathBuf::from(p),
            None => server_path(detected.lang.server_bin, &install_dir),
        };
        let args = (detected.lang.server_args)(&root);

        let mut info = ManagedServerInfo {
            project_root: root.clone(),
            language: detected.lang.name.to_string(),
            status: "starting".into(),
            idle_since: now_ms(),
            pid: None,
            just_started: true,
            loading: false,
        };

        let client_res = LspClient::spawn(&bin.to_string_lossy(), &args, &root).await;
        let mut client = match client_res {
            Ok(mut c) => match c.initialize(&root).await {
                Ok(_) => {
                    info.status = "running".into();
                    info.pid = c.pid();
                    c
                }
                // No `info.status = "stopped"` on these paths: `info` is a
                // local that is dropped immediately, so the write never
                // reached `servers`, `list()`, or the caller. It read as
                // if failed servers were tracked when nothing tracks them.
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        };

        // Wait for the initial index here, while still holding the per-key
        // create lock, so *every* caller that asked for this project gets a
        // server that can already answer — not just whichever one happened
        // to spawn it. Doing this on the CLI side instead meant a second
        // command arriving during a cold start saw an entry that existed,
        // treated it as warm, and queried a server that was still loading.
        wait_until_indexed(&mut client, detected.lang.name, file_path).await;
        info.loading = client.is_busy();

        self.watcher
            .ensure_watching(&root, detected.lang.extensions)
            .await;

        // The stored copy describes the server, not this response: it was
        // listed by `server list` with `just_started: true` forever.
        let entry = ManagedServer {
            client: Arc::new(Mutex::new(client)),
            info: ManagedServerInfo {
                just_started: false,
                loading: false,
                ..info.clone()
            },
        };
        self.servers.lock().await.insert(key, entry);
        Ok(info)
    }

    /// Sends `method`/`params` as a notification to every running server
    /// for `project_root` — used by the file watcher to push
    /// `workspace/didChangeWatchedFiles` batches. Best-effort: a send
    /// failure on one server doesn't stop delivery to the others.
    pub async fn broadcast_notify(&self, project_root: &str, method: &str, params: Value) {
        // Snapshot the client handles, then release `servers` before
        // notifying. Holding the map lock across `client.lock().await`
        // meant a single in-flight request (which holds its client lock for
        // up to the 120s request ceiling) blocked every other project's
        // `/list`, `/create`, and `/request` for as long as it ran. This is
        // the file-watcher path, so it fires on every debounced batch.
        let targets: Vec<Arc<Mutex<LspClient>>> = {
            let servers = self.servers.lock().await;
            servers
                .values()
                .filter(|s| s.info.project_root == project_root && s.info.status == "running")
                .map(|s| s.client.clone())
                .collect()
        };
        for client in targets {
            let _ = client.lock().await.notify(method, params.clone()).await;
        }
    }

    /// Sends a request to the (single) running server matching
    /// `project_root` and, if given, `language`, and returns its result —
    /// this is what lets the navigation commands reuse a warm server
    /// instead of spawning their own. Touches `idle_since` first, so a
    /// request in flight always counts as activity even if it's slow.
    ///
    /// Two methods get non-generic handling before falling through to a
    /// plain proxied request — `diagnostic_with_push_fallback` and (in
    /// `proxy_notify`) `didopen_as_sync_document`. Both exist because the
    /// generic "just forward it" contract breaks down for these two
    /// specific methods when servers are warm and reused across calls; see
    /// each helper's doc comment for why. Adding a third such case should
    /// follow the same pattern — a private, well-named helper dispatched
    /// from here — rather than growing this match arm-by-arm.
    pub async fn proxy_request(
        &self,
        project_root: &str,
        language: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let client = self.find_running_client(project_root, language).await?;
        let mut c = client.lock().await;
        // Files edited on disk since the server last saw them, other than
        // through this tool, would otherwise be answered from stale memory.
        c.resync_from_disk(None).await?;

        if method == "textDocument/diagnostic" {
            return Self::diagnostic_with_push_fallback(&mut c, method, &params).await;
        }

        c.request(method, params).await
    }

    /// LSP 3.17 pull diagnostics aren't universally supported —
    /// typescript-language-server in particular only ever pushes
    /// `publishDiagnostics` notifications and answers a pull request with
    /// "method not found". Fall back to whatever's been pushed and cached
    /// for this URI instead of surfacing that as a hard failure.
    async fn diagnostic_with_push_fallback(
        c: &mut LspClient,
        method: &str,
        params: &Value,
    ) -> Result<Value> {
        match c.request(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e)
                if crate::lsp_client::is_rpc_error_code(
                    &e,
                    crate::lsp_client::METHOD_NOT_FOUND,
                ) =>
            {
                let uri = params
                    .get("textDocument")
                    .and_then(|t| t.get("uri"))
                    .and_then(|u| u.as_str())
                    .unwrap_or_default()
                    .to_string();
                // Push diagnostics arrive whenever the server finishes
                // analysing, so after an edit the cached set is stale until
                // the next publish. Wait for one that postdates the latest
                // change, but only as long as an analysis pass plausibly
                // takes. typescript-language-server re-publishes a file only
                // when its diagnostics changed, so after an edit that didn't
                // affect this file nothing arrives: a file that has
                // published before gets the shorter wait, and the wait is
                // recorded so the next call with nothing new skips it.
                c.drain_pending_notifications().await;
                if !c.diagnostics_are_current(&uri) {
                    let wait = if c.publish_count(&uri) > 0 {
                        PUSH_DIAGNOSTICS_REPUBLISH_WAIT
                    } else {
                        PUSH_DIAGNOSTICS_WAIT
                    };
                    let deadline = tokio::time::Instant::now() + wait;
                    c.pump_until(wait, |c| c.diagnostics_are_current(&uri))
                        .await;
                    // Servers publish in stages — typescript-language-
                    // server sends syntactic diagnostics first and the
                    // (slower) semantic ones in a later publish — so the
                    // first publish after an edit can be an incomplete,
                    // often empty, set. Keep listening until the file has
                    // been quiet for a moment.
                    loop {
                        let seen = c.publish_count(&uri);
                        let remaining =
                            deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        c.pump_until(PUSH_DIAGNOSTICS_QUIET.min(remaining), |c| {
                            c.publish_count(&uri) != seen
                        })
                        .await;
                        if c.publish_count(&uri) == seen {
                            break;
                        }
                    }
                    c.mark_diagnostics_waited(&uri);
                }
                Ok(serde_json::json!({ "items": c.cached_diagnostics(&uri) }))
            }
            Err(e) => Err(e),
        }
    }

    /// Same as `proxy_request` but for a notification (`textDocument/
    /// didOpen`, etc.). Returns whether the server's view of the project's
    /// open documents changed — always `true` for anything but `didOpen`,
    /// which is the one this tool can tell is a no-op.
    pub async fn proxy_notify(
        &self,
        project_root: &str,
        language: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<bool> {
        let client = self.find_running_client(project_root, language).await?;
        let mut c = client.lock().await;

        if method == "textDocument/didOpen" {
            return Self::didopen_as_sync_document(&mut c, &params).await;
        }

        c.notify(method, params).await?;
        Ok(true)
    }

    /// See `LspClient::sync_document` — every navigation command "opens"
    /// its target file unconditionally, but with warm server reuse the
    /// file may already be open from an earlier call, so this needs to
    /// become a `didChange` instead of a second `didOpen`.
    async fn didopen_as_sync_document(c: &mut LspClient, params: &Value) -> Result<bool> {
        let uri = params
            .get("textDocument")
            .and_then(|t| t.get("uri"))
            .and_then(|u| u.as_str())
            .unwrap_or_default();
        let language_id = params
            .get("textDocument")
            .and_then(|t| t.get("languageId"))
            .and_then(|l| l.as_str())
            .unwrap_or_default();
        let text = params
            .get("textDocument")
            .and_then(|t| t.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let others = c.resync_from_disk(Some(uri)).await?;
        let this = c.sync_document(uri, language_id, text).await?;
        Ok(others || this)
    }

    async fn find_running_client(
        &self,
        project_root: &str,
        language: Option<&str>,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let mut servers = self.servers.lock().await;
        let entry = servers
            .values_mut()
            .find(|s| {
                s.info.project_root == project_root
                    && language.is_none_or(|l| s.info.language == l)
                    && s.info.status == "running"
            })
            .ok_or_else(|| anyhow::anyhow!("No server running for project: {project_root}"))?;
        entry.info.idle_since = now_ms();
        Ok(entry.client.clone())
    }

    /// Stops the file watcher for `project_root` if no server is left
    /// running for it — call after removing entries from `servers`.
    async fn stop_watcher_if_unused(&self, project_root: &str) {
        let still_used = self
            .servers
            .lock()
            .await
            .values()
            .any(|s| s.info.project_root == project_root);
        if !still_used {
            self.watcher.stop(project_root).await;
        }
    }

    /// Stop and evict any server whose idle_since is older than `timeout`,
    /// or whose process has died on its own (crashed/OOM-killed) since we
    /// last checked — the latter needs no waiting for a timeout at all,
    /// since there's nothing left to gracefully shut down.
    pub async fn reap_idle(&self, timeout: std::time::Duration) {
        let now = now_ms();
        let cutoff_ms = timeout.as_millis() as i64;

        // Snapshot (key, client, idle_since) and release the `servers` lock
        // before the liveness-check awaits below. This runs on a 30s timer
        // against every warm server — holding `servers` locked across N
        // sequential `is_alive()` awaits would block `proxy_request`/
        // `create` (which also need that lock) for the whole scan instead
        // of the brief snapshot copy they actually require.
        let snapshot: Vec<(String, Arc<Mutex<LspClient>>, i64)> = {
            let servers = self.servers.lock().await;
            servers
                .iter()
                .filter(|(_, s)| s.info.status == "running")
                .map(|(key, s)| (key.clone(), s.client.clone(), s.info.idle_since))
                .collect()
        };

        let mut stale_keys = Vec::new();
        for (key, client, idle_since) in snapshot {
            let alive = client.lock().await.is_alive();
            if !alive || is_stale(idle_since, now, cutoff_ms) {
                stale_keys.push(key);
            }
        }

        let removed: Vec<ManagedServer> = {
            let mut servers = self.servers.lock().await;
            stale_keys
                .into_iter()
                .filter_map(|key| servers.remove(&key))
                .collect()
        };

        let mut removed_roots = Vec::new();
        for entry in removed {
            entry.client.lock().await.shutdown().await;
            removed_roots.push(entry.info.project_root);
        }
        for root in removed_roots {
            self.stop_watcher_if_unused(&root).await;
        }

        // Drop search indexes for projects that have gone quiet. Each one
        // holds every symbol in its project in memory, so they shouldn't
        // outlive interest in the project any more than a warm server does.
        self.search_indexes
            .lock()
            .await
            .retain(|_, entry| !is_stale(entry.used_at, now, cutoff_ms));
    }

    /// BM25 fallback search, reusing a cached index when the tree has not
    /// changed since it was built.
    ///
    /// The fingerprint is computed *outside* the lock: it is a stat walk,
    /// and holding the index map across it would serialize unrelated
    /// projects' searches behind each other.
    pub async fn search(
        &self,
        project_root: &str,
        query: &str,
    ) -> Vec<crate::protocol::SymbolInformation> {
        let fingerprint = {
            let root = project_root.to_string();
            // Blocking filesystem work; keep it off the async runtime's
            // worker so a large tree doesn't stall other requests.
            tokio::task::spawn_blocking(move || crate::bm25::TreeFingerprint::of(&root))
                .await
                .unwrap_or_else(|_| crate::bm25::TreeFingerprint::of(project_root))
        };

        let cached = {
            let mut indexes = self.search_indexes.lock().await;
            match indexes.get_mut(project_root) {
                Some(entry) if entry.fingerprint == fingerprint => {
                    entry.used_at = now_ms();
                    Some(entry.index.clone())
                }
                _ => None,
            }
        };

        let index = match cached {
            Some(index) => index,
            None => {
                let root = project_root.to_string();
                let built =
                    tokio::task::spawn_blocking(move || crate::bm25::Bm25Index::build(&root))
                        .await
                        .unwrap_or_else(|_| crate::bm25::Bm25Index::build(project_root));
                let index = Arc::new(built);
                self.search_indexes.lock().await.insert(
                    project_root.to_string(),
                    CachedIndex {
                        index: index.clone(),
                        fingerprint,
                        used_at: now_ms(),
                    },
                );
                index
            }
        };

        index
            .search(query)
            .into_iter()
            .map(|(_, sym)| sym.clone())
            .collect()
    }

    pub async fn delete(&self, req: DeleteRequest) -> Vec<ManagedServerInfo> {
        // Remove the entries under the lock, then shut them down after
        // releasing it — the same pattern `reap_idle` uses. `shutdown()`
        // awaits a 3s timeout per server, so doing this under the map lock
        // made `lsp server stop --all` with five warm servers hold the one
        // `servers` mutex for up to 15 seconds, blocking every concurrent
        // request the daemon was serving.
        let removed: Vec<ManagedServer> = {
            let mut servers = self.servers.lock().await;
            let keys: Vec<String> = if req.all {
                servers.keys().cloned().collect()
            } else if let Some(path) = &req.path {
                servers
                    .iter()
                    .filter(|(k, s)| {
                        &s.info.project_root == path || k.starts_with(&format!("{path}::"))
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            } else {
                vec![]
            };
            keys.into_iter()
                .filter_map(|k| servers.remove(&k))
                .collect()
        };

        let mut stopped = Vec::new();
        for entry in removed {
            entry.client.lock().await.shutdown().await;
            stopped.push(entry.info);
        }
        let mut seen_roots = std::collections::HashSet::new();
        for info in &stopped {
            if seen_roots.insert(info.project_root.clone()) {
                self.stop_watcher_if_unused(&info.project_root).await;
            }
        }
        stopped
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// How long a freshly spawned server is given to finish its initial index,
/// and how often readiness is probed while waiting.
///
/// There is no portable "indexing finished" notification in LSP, so this
/// polls for the thing callers actually need: `documentSymbol` on the file
/// that triggered the spawn returning something. Servers differ by an order
/// of magnitude here — typescript-language-server answers almost at once,
/// gopls reports `no package metadata` until its initial load completes,
/// rust-analyzer takes longer still — so waiting for the observed condition
/// beats any constant large enough for the slowest of them.
/// Longest `diagnostics` waits for a server that only pushes diagnostics
/// to publish a settled set newer than the latest edit. Paid only by a
/// diagnostics call that follows an edit, and only until the publishes
/// stop; it replaces the 3s the CLI used to sleep before every command.
const PUSH_DIAGNOSTICS_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The wait for a file that has published diagnostics before, and so will
/// only publish again if they changed. Covers a re-check pass of a warm
/// server; if nothing arrives by then, the cached set still stands.
const PUSH_DIAGNOSTICS_REPUBLISH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a file must go without a new `publishDiagnostics` before the
/// set is taken as final. See `diagnostic_with_push_fallback`.
const PUSH_DIAGNOSTICS_QUIET: std::time::Duration = std::time::Duration::from_millis(750);

const INDEX_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const INDEX_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// Blocks until `client` can answer `documentSymbol` for `file_path`, or
/// the timeout elapses.
///
/// Best-effort by design: every phase is bounded, and it runs exactly once
/// per spawned server, never on the warm path.
/// Skipped for the bundled tree-sitter servers, which parse synchronously
/// inside the request handler and have no indexing phase at all.
async fn wait_until_indexed(client: &mut LspClient, language: &str, file_path: &std::path::Path) {
    if crate::registry::is_bundled_server(
        crate::registry::languages()
            .iter()
            .find(|l| l.name == language)
            .map(|l| l.server_bin)
            .unwrap_or(""),
    ) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file_path) else {
        return; // a bare directory, or an unreadable file: nothing to probe with
    };
    let uri = lsp::uri::from_path(file_path);
    if client
        .sync_document(
            &uri,
            crate::project::language_id(language, file_path),
            &text,
        )
        .await
        .is_err()
    {
        return;
    }

    let deadline = std::time::Instant::now() + INDEX_READY_TIMEOUT;

    // Phase 1: syntax. `documentSymbol` is answered from the parse alone,
    // so this returns as soon as the file has been read — which is what
    // `outline` needs, and no more than that.
    if let Some(probe_position) = poll_document_symbol(client, &uri, deadline).await {
        wait_for_hover(client, &uri, probe_position, deadline).await;
    }

    // Phase 3: project load. A hover that answers proves the file is
    // parsed, not that the rest of the project is loaded: a cold
    // typescript-language-server resolves a definition only as far as the
    // local import binding until it finishes "Initializing JS/TS language
    // features". Servers announce that as work-done progress (advertised
    // in `initialize`), so wait for it to end. Progress may begin a moment
    // after the first answers, hence the short grace period before
    // concluding a server doesn't report any.
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    client
        .pump_until(PROGRESS_GRACE.min(remaining), |c| c.reports_progress())
        .await;
    let remaining = deadline
        .saturating_duration_since(std::time::Instant::now())
        .min(PROJECT_LOAD_WAIT);
    client.pump_until(remaining, |c| !c.is_busy()).await;
}

/// Cap on phase 3 of `wait_until_indexed`. Loading the project's own
/// sources is seconds; what can take far longer is work a navigation
/// command rarely needs — rust-analyzer running every build script and
/// proc-macro through `cargo check`, a progress token a buggy server never
/// ends. Past this, the server is handed out still loading and reported as
/// such (`ManagedServerInfo::loading`), so the CLI keeps retrying answers
/// that look incomplete.
const PROJECT_LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

async fn wait_for_hover(
    client: &mut LspClient,
    uri: &str,
    probe_position: Value,
    deadline: std::time::Instant,
) {
    // Phase 2: types. This is the part `documentSymbol` does not prove.
    // gopls answers hover and definition with `no package metadata for
    // file` until its initial package load finishes, while happily
    // answering documentSymbol throughout — so stopping after phase 1 hands
    // back a server that can outline but cannot do anything type-aware, and
    // the caller's first `doc` or `definition` fails.
    //
    // The signal is the *shape* of the reply, not its content: a server
    // that is still loading returns an error, while one that is ready
    // returns a result — possibly `null`, if there is genuinely nothing to
    // say about that position. So this stops on any `Ok`, which also means
    // it cannot spin on a symbol that simply has no hover text.
    let hover = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": probe_position,
    });
    while std::time::Instant::now() < deadline {
        if client
            .request("textDocument/hover", hover.clone())
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(INDEX_POLL_INTERVAL).await;
    }
}

/// How long a freshly started server gets to report its first progress
/// before the readiness wait assumes it doesn't report any.
const PROGRESS_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Polls `documentSymbol` until the server returns at least one symbol, and
/// yields a position inside the first one to probe type-readiness with.
async fn poll_document_symbol(
    client: &mut LspClient,
    uri: &str,
    deadline: std::time::Instant,
) -> Option<Value> {
    let params = serde_json::json!({ "textDocument": { "uri": uri } });
    let mut empty_since: Option<std::time::Instant> = None;
    while std::time::Instant::now() < deadline {
        // An error here means "still loading" for several servers, so it is
        // a reason to keep waiting rather than to stop.
        if let Ok(v) = client
            .request("textDocument/documentSymbol", params.clone())
            .await
        {
            if let Some(pos) = first_symbol_position(&v) {
                return Some(pos);
            }
            // A successful empty answer is either "still loading" or "this
            // file has no symbols" (a barrel file of re-exports, an empty
            // module). Waiting for symbols that will never come used to
            // cost such a file the full 60s on every cold start. Stop once
            // the server says it is idle, or once the empty answer has held
            // long enough to be believed.
            let since = *empty_since.get_or_insert_with(std::time::Instant::now);
            client.drain_pending_notifications().await;
            if (client.reports_progress() && !client.is_busy())
                || since.elapsed() >= EMPTY_OUTLINE_BELIEVED_AFTER
            {
                return None;
            }
        }
        tokio::time::sleep(INDEX_POLL_INTERVAL).await;
    }
    None
}

/// How long `documentSymbol` must keep answering an empty list, from a
/// server that doesn't report progress, before the file is taken to have
/// no symbols.
const EMPTY_OUTLINE_BELIEVED_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

/// Start of the first symbol's name in a `documentSymbol` reply.
///
/// Handles both response shapes: hierarchical `DocumentSymbol[]`
/// (`selectionRange`) and flat `SymbolInformation[]` (`location.range`).
fn first_symbol_position(result: &Value) -> Option<Value> {
    let first = result.as_array()?.first()?;
    let range = first
        .get("selectionRange")
        .or_else(|| first.get("range"))
        .or_else(|| first.get("location").and_then(|l| l.get("range")))?;
    range.get("start").cloned()
}

/// How long the shutdown handler waits before calling `process::exit`, so
/// the HTTP 204 makes it back to the client that asked for the shutdown.
const EXIT_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

/// How often the idle reaper scans for servers to evict.
/// How long an exiting daemon waits for connections already accepted to
/// finish.
const EXIT_DRAIN_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Pure predicate extracted out of reap_idle for direct unit testing.
fn is_stale(idle_since: i64, now: i64, cutoff_ms: i64) -> bool {
    now - idle_since > cutoff_ms
}

pub use crate::paths::socket_path;

type SharedManager = Arc<Manager>;

async fn list_handler(State(m): State<SharedManager>) -> Json<Vec<ManagedServerInfo>> {
    Json(m.list().await)
}

async fn create_handler(
    State(m): State<SharedManager>,
    Json(req): Json<CreateRequest>,
) -> Result<Json<ManagedServerInfo>, (axum::http::StatusCode, String)> {
    m.create(
        &req.path,
        req.project_root.as_deref(),
        req.server_path.as_deref(),
        req.language.as_deref(),
    )
    .await
    .map(Json)
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn delete_handler(
    State(m): State<SharedManager>,
    Json(req): Json<DeleteRequest>,
) -> Json<Vec<ManagedServerInfo>> {
    Json(m.delete(req).await)
}

async fn request_handler(
    State(m): State<SharedManager>,
    Json(req): Json<ProxyRequest>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    m.proxy_request(
        &req.project_root,
        req.language.as_deref(),
        &req.method,
        req.params,
    )
    .await
    .map(Json)
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn notify_handler(
    State(m): State<SharedManager>,
    Json(req): Json<ProxyRequest>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    m.proxy_notify(
        &req.project_root,
        req.language.as_deref(),
        &req.method,
        req.params,
    )
    .await
    .map(|changed| Json(serde_json::json!({ "changed": changed })))
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn search_handler(
    State(m): State<SharedManager>,
    Json(req): Json<SearchRequest>,
) -> Json<Vec<crate::protocol::SymbolInformation>> {
    Json(m.search(&req.project_root, &req.query).await)
}

/// The socket this daemon bound: its path and inode.
static BOUND_SOCKET: std::sync::OnceLock<(std::path::PathBuf, u64)> = std::sync::OnceLock::new();

/// Whether the socket file at our path is still the one this daemon bound.
/// `false` once it has been deleted or replaced — by a newer daemon, or by
/// someone removing the state directory — after which no client can ever
/// reach this daemon again.
fn still_own_socket() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some((path, ino)) = BOUND_SOCKET.get() else {
        return true;
    };
    std::fs::symlink_metadata(path).is_ok_and(|m| m.ino() == *ino)
}

/// When the daemon last accepted a connection (ms since the epoch), and
/// how many connections are being served right now. See `should_exit_idle`.
static LAST_ACTIVITY_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts one connection as in flight for as long as it lives.
struct InFlight;

impl InFlight {
    fn start() -> Self {
        LAST_ACTIVITY_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        InFlight
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        LAST_ACTIVITY_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether a daemon with no servers should exit: nothing in flight, and no
/// connection for longer than `idle`. Split out for testing.
fn should_exit_idle(
    servers: usize,
    in_flight: usize,
    last_activity: i64,
    now: i64,
    idle: i64,
) -> bool {
    servers == 0 && in_flight == 0 && now - last_activity > idle
}

/// Removes the socket file, but only if it is still the one this daemon
/// bound. Once a shutting-down daemon has given up its path, a new daemon
/// may already have bound a fresh socket there; deleting *that* would
/// orphan the new daemon.
fn remove_own_socket() {
    use std::os::unix::fs::MetadataExt;
    let Some((path, ino)) = BOUND_SOCKET.get() else {
        return;
    };
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.ino() == *ino) {
        let _ = std::fs::remove_file(path);
    }
}

impl Manager {
    /// Shuts down every server and watcher. The caller gives up the socket
    /// first, so a client arriving meanwhile starts a new daemon instead of
    /// reaching this dying one.
    async fn shutdown_everything(&self) {
        // Snapshot then release, as in `delete`/`reap_idle`: each
        // `shutdown()` awaits up to 3s, and holding the map lock across
        // all of them blocks any request still in flight.
        let clients: Vec<Arc<Mutex<LspClient>>> = {
            let servers = self.servers.lock().await;
            servers.values().map(|s| s.client.clone()).collect()
        };
        for client in clients {
            client.lock().await.shutdown().await;
        }
        self.watcher.dispose().await;
    }

    async fn server_count(&self) -> usize {
        self.servers.lock().await.len()
    }
}

async fn shutdown_handler(
    State(m): State<SharedManager>,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    // `{"if_build": X}`: shut down only if this daemon is build X (the one
    // the caller saw and wants replaced). See `ManagerClient::ensure_running`.
    let wanted = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("if_build").cloned());
    match wanted {
        Some(Value::String(build)) if build != crate::paths::build_id() => {
            return axum::http::StatusCode::CONFLICT;
        }
        // The caller saw a daemon with no build id; this one has one.
        Some(Value::Null) => return axum::http::StatusCode::CONFLICT,
        _ => {}
    }
    // Give up the socket path first. It used to stay bound until after
    // every server had shut down and a grace period had passed, so a
    // command issued right after `server shutdown` found the old daemon
    // still answering `is_alive`, then lost it mid-command ("cannot reach
    // manager daemon") instead of starting a new one. The connection this
    // request arrived on is unaffected.
    remove_own_socket();
    m.shutdown_everything().await;
    tokio::spawn(async {
        tokio::time::sleep(EXIT_GRACE_PERIOD).await;
        std::process::exit(0);
    });
    axum::http::StatusCode::NO_CONTENT
}

pub fn app(manager: SharedManager) -> Router {
    Router::new()
        .route("/list", get(list_handler))
        .route("/create", post(create_handler))
        .route("/delete", delete(delete_handler))
        .route("/search", post(search_handler))
        .route("/request", post(request_handler))
        .route("/notify", post(notify_handler))
        .route("/shutdown", post(shutdown_handler))
        .with_state(manager)
}

/// Entry point for `lsp --daemon`. Removes any stale socket, binds a fresh
/// UnixListener, and serves the manager API until SIGTERM/SIGINT.
///
/// `ManagerClient::ensure_running` already serializes concurrent spawns
/// across processes with a lock file, so in practice only one `--daemon`
/// process should ever reach this function at a time. This connect-before-
/// remove check is defense in depth for that invariant: unconditionally
/// deleting the socket path (the old behavior) would delete out from under
/// any *other* daemon that's genuinely still alive and serving on it,
/// permanently orphaning it (it keeps running, listening on an unlinked
/// inode, unreachable and unkillable via `lsp server shutdown` since that
/// can only ever reach whichever daemon currently owns the live path) —
/// reproduced live during review by racing concurrent daemon spawns.
pub async fn start_daemon() -> Result<()> {
    let cfg = crate::config::load_config();
    let path = socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // The daemon speaks an unauthenticated HTTP API (create/delete/
        // request/notify/shutdown) over this socket — anyone who can open
        // it can read file contents this process can access (via hover/
        // definition) or kill/spawn language servers. Default directory
        // permissions are umask-derived (commonly 0755, world-readable);
        // restrict to the owner only, matching what a private control
        // socket should be regardless of umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    if let Err(e) = crate::paths::check_socket_path(&path) {
        // Also checked CLI-side before spawning; repeated here because the
        // daemon can be started directly with `lsp --daemon`.
        eprintln!("[daemon] cannot bind: {e}");
        anyhow::bail!("{e}");
    }

    if tokio::net::UnixStream::connect(&path).await.is_ok() {
        // Another daemon is alive and already serving this socket — do not
        // touch it or bind our own listener. Exit quietly; the caller's
        // `ensure_running` will find the existing daemon via `is_alive()`.
        return Ok(());
    }
    let _ = std::fs::remove_file(&path);

    let listener = tokio::net::UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let _ = BOUND_SOCKET.set((path.clone(), std::fs::symlink_metadata(&path)?.ino()));
    }
    let (manager, mut watch_rx) = Manager::new();
    let manager: SharedManager = Arc::new(manager);

    let idle_manager = manager.clone();
    let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout);
    let daemon_idle_ms = cfg.daemon_idle_timeout.unwrap_or(cfg.idle_timeout) as i64 * 1000;
    // Often enough to honour a short daemon idle timeout, never more often
    // than once a second.
    let reap_interval = REAP_INTERVAL.min(std::time::Duration::from_millis(
        (daemon_idle_ms as u64 / 2).max(1000),
    ));
    LAST_ACTIVITY_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(reap_interval).await;
            idle_manager.reap_idle(idle_timeout).await;
            // The daemon used to run forever. One whose socket was deleted
            // or replaced can never be reached again, and one with nothing
            // to serve for a long time is only holding memory; both exit.
            // (Test runs left dozens behind, one per state directory.)
            let orphaned = !still_own_socket();
            let idle = should_exit_idle(
                idle_manager.server_count().await,
                IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed),
                LAST_ACTIVITY_MS.load(std::sync::atomic::Ordering::Relaxed),
                now_ms(),
                daemon_idle_ms,
            );
            if orphaned || idle {
                eprintln!(
                    "[daemon] exiting: {}",
                    if orphaned {
                        "its socket is gone or was replaced"
                    } else {
                        "idle"
                    }
                );
                // Give up the path first, so any client from here on starts
                // a fresh daemon; then let connections accepted just before
                // that finish, rather than cutting them off mid-response.
                remove_own_socket();
                let drain_by = std::time::Instant::now() + EXIT_DRAIN_LIMIT;
                while IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed) > 0
                    && std::time::Instant::now() < drain_by
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                idle_manager.shutdown_everything().await;
                std::process::exit(0);
            }
        }
    });

    // Forwards debounced file-watcher batches to the servers that care
    // about them — see the `watcher` field doc comment on `Manager`.
    let watch_manager = manager.clone();
    tokio::spawn(async move {
        while let Some((root, changes)) = watch_rx.recv().await {
            watch_manager
                .broadcast_notify(
                    &root,
                    "workspace/didChangeWatchedFiles",
                    serde_json::json!({ "changes": changes }),
                )
                .await;
        }
    });

    let router = app(manager);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // The daemon outlives the terminal that started it. Without a handler,
    // SIGHUP's default action killed it, skipping the socket cleanup below.
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::spawn(async move { while sighup.recv().await.is_some() {} });
    tokio::select! {
        res = serve_uds(listener, router) => { res?; }
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    remove_own_socket();
    Ok(())
}

async fn serve_uds(listener: tokio::net::UnixListener, router: Router) -> Result<()> {
    loop {
        // A failed `accept` is about that one connection (or a transient
        // resource limit such as EMFILE), not the listener. Returning it
        // used to shut the whole daemon down, taking every warm server
        // with it. Back off briefly so a persistent condition can't spin.
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("[daemon] accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let router = router.clone();
        let guard = InFlight::start();
        tokio::spawn(async move {
            // Dropped when the connection ends, however it ends — a handler
            // that panics aborts this task, and a counter decremented by a
            // statement after the await would then never come back down,
            // leaving the daemon unable to ever go idle.
            let _guard = guard;
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |req| {
                let router = router.clone();
                async move {
                    let mut response = tower::ServiceExt::oneshot(router, req).await.unwrap();
                    // Every response says which build answered, so a CLI
                    // from a different build can replace this daemon
                    // instead of being served by stale code
                    // (`ManagerClient::ensure_running`).
                    if let Ok(value) = axum::http::HeaderValue::from_str(crate::paths::build_id()) {
                        response
                            .headers_mut()
                            .insert(crate::paths::BUILD_HEADER, value);
                    }
                    Ok::<_, std::convert::Infallible>(response)
                }
            });
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, service)
                    .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `is_stale` carries a doc comment saying it was "extracted out of
    // reap_idle for direct unit testing" — and then this file had no test
    // module at all. These are those tests.

    // --- readiness probe ---------------------------------------------

    #[test]
    fn first_symbol_position_reads_the_hierarchical_shape() {
        // DocumentSymbol[]: the name's own range is `selectionRange`.
        let v = serde_json::json!([{
            "name": "User",
            "kind": 23,
            "range": { "start": {"line": 2, "character": 0}, "end": {"line": 5, "character": 1} },
            "selectionRange": { "start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 9} }
        }]);
        let pos = first_symbol_position(&v).unwrap();
        assert_eq!(pos["line"], 2);
        assert_eq!(pos["character"], 5);
    }

    #[test]
    fn first_symbol_position_reads_the_flat_shape() {
        // SymbolInformation[]: no `range` at the top level, only
        // `location.range`.
        let v = serde_json::json!([{
            "name": "User",
            "kind": 23,
            "location": {
                "uri": "file:///a.go",
                "range": { "start": {"line": 7, "character": 3}, "end": {"line": 7, "character": 7} }
            }
        }]);
        let pos = first_symbol_position(&v).unwrap();
        assert_eq!(pos["line"], 7);
        assert_eq!(pos["character"], 3);
    }

    #[test]
    fn first_symbol_position_is_none_when_there_is_nothing_to_probe() {
        assert!(first_symbol_position(&serde_json::json!([])).is_none());
        assert!(first_symbol_position(&serde_json::Value::Null).is_none());
        assert!(first_symbol_position(&serde_json::json!([{ "name": "x" }])).is_none());
    }

    #[test]
    fn the_daemon_exits_only_when_empty_quiet_and_idle_long_enough() {
        // No servers, nothing in flight, idle past the limit: exit.
        assert!(should_exit_idle(0, 0, 1_000, 12_000, 10_000));
        // Any one of those not holding keeps it alive.
        assert!(!should_exit_idle(1, 0, 1_000, 12_000, 10_000));
        assert!(!should_exit_idle(0, 1, 1_000, 12_000, 10_000));
        assert!(!should_exit_idle(0, 0, 5_000, 12_000, 10_000));
    }

    #[test]
    fn a_server_idle_longer_than_the_cutoff_is_stale() {
        let now = 1_000_000;
        assert!(is_stale(now - 5_000, now, 1_000));
    }

    #[test]
    fn a_server_idle_less_than_the_cutoff_is_not_stale() {
        let now = 1_000_000;
        assert!(!is_stale(now - 500, now, 1_000));
    }

    #[test]
    fn the_cutoff_boundary_is_exclusive() {
        // Exactly at the cutoff is not yet stale, so a server used exactly
        // idleTimeout ago survives one more scan rather than being reaped
        // on the tick it becomes eligible.
        let now = 1_000_000;
        assert!(!is_stale(now - 1_000, now, 1_000));
        assert!(is_stale(now - 1_001, now, 1_000));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_report_stale() {
        // now_ms comes from SystemTime, which can move backwards across an
        // NTP correction. A negative age must not be treated as "very old".
        let now = 1_000_000;
        assert!(!is_stale(now + 10_000, now, 1_000));
    }

    #[test]
    fn a_zero_cutoff_reaps_anything_with_any_measurable_idle_time() {
        let now = 1_000_000;
        assert!(is_stale(now - 1, now, 0));
        assert!(!is_stale(now, now, 0));
    }
}
