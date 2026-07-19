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
use crate::registry::{default_install_dir, detect_project_root, server_path};
use crate::watcher::WatcherManager;
use anyhow::Result;
use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
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
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub path: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteRequest {
    pub path: Option<String>,
    #[serde(default)]
    pub all: bool,
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
}

impl Manager {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<crate::watcher::WatchBatch>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                servers: Mutex::new(HashMap::new()),
                create_locks: Mutex::new(HashMap::new()),
                watcher: WatcherManager::new(tx),
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

    pub async fn create(&self, path: &str) -> Result<ManagedServerInfo> {
        let file_path = std::path::Path::new(path);
        let detected = detect_project_root(file_path)
            .or_else(|| {
                // Allow `path` to be a bare directory: probe common entry files.
                for probe in ["index.ts", "main.go", "main.py", "main.rs", "Main.java"] {
                    if let Some(d) = detect_project_root(&file_path.join(probe)) {
                        return Some(d);
                    }
                }
                None
            })
            .ok_or_else(|| anyhow::anyhow!("Cannot detect language for path: {path}"))?;

        let root = detected.root.to_string_lossy().to_string();
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
        {
            let mut servers = self.servers.lock().await;
            if let Some(existing) = servers.get_mut(&key) {
                let alive = existing.client.lock().await.is_alive();
                if alive {
                    existing.info.idle_since = now_ms();
                    return Ok(existing.info.clone());
                }
                servers.remove(&key);
            }
        }

        let install_dir = default_install_dir();
        let bin = server_path(detected.lang.server_bin, &install_dir);
        let args = (detected.lang.server_args)(&root);

        let mut info = ManagedServerInfo {
            project_root: root.clone(),
            language: detected.lang.name.to_string(),
            status: "starting".into(),
            idle_since: now_ms(),
            pid: None,
        };

        let client_res = LspClient::spawn(&bin.to_string_lossy(), &args, &root).await;
        let client = match client_res {
            Ok(mut c) => match c.initialize(&root).await {
                Ok(_) => {
                    info.status = "running".into();
                    info.pid = c.pid();
                    c
                }
                Err(e) => {
                    info.status = "stopped".into();
                    return Err(e);
                }
            },
            Err(e) => {
                info.status = "stopped".into();
                return Err(e);
            }
        };

        self.watcher
            .ensure_watching(&root, detected.lang.extensions)
            .await;

        let entry = ManagedServer {
            client: Arc::new(Mutex::new(client)),
            info: info.clone(),
        };
        self.servers.lock().await.insert(key, entry);
        Ok(info)
    }

    /// Sends `method`/`params` as a notification to every running server
    /// for `project_root` — used by the file watcher to push
    /// `workspace/didChangeWatchedFiles` batches. Best-effort: a send
    /// failure on one server doesn't stop delivery to the others.
    pub async fn broadcast_notify(&self, project_root: &str, method: &str, params: Value) {
        let servers = self.servers.lock().await;
        for s in servers.values() {
            if s.info.project_root == project_root && s.info.status == "running" {
                let _ = s.client.lock().await.notify(method, params.clone()).await;
            }
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
                c.drain_pending_notifications().await;
                let uri = params
                    .get("textDocument")
                    .and_then(|t| t.get("uri"))
                    .and_then(|u| u.as_str())
                    .unwrap_or_default();
                Ok(serde_json::json!({ "items": c.cached_diagnostics(uri) }))
            }
            Err(e) => Err(e),
        }
    }

    /// Same as `proxy_request` but for a fire-and-forget notification
    /// (`textDocument/didOpen`, etc.) — no result to return.
    pub async fn proxy_notify(
        &self,
        project_root: &str,
        language: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<()> {
        let client = self.find_running_client(project_root, language).await?;
        let mut c = client.lock().await;

        if method == "textDocument/didOpen" {
            return Self::didopen_as_sync_document(&mut c, &params).await;
        }

        c.notify(method, params).await
    }

    /// See `LspClient::sync_document` — every navigation command "opens"
    /// its target file unconditionally, but with warm server reuse the
    /// file may already be open from an earlier call, so this needs to
    /// become a `didChange` instead of a second `didOpen`.
    async fn didopen_as_sync_document(c: &mut LspClient, params: &Value) -> Result<()> {
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
        c.sync_document(uri, language_id, text).await
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
    }

    pub async fn delete(&self, req: DeleteRequest) -> Vec<ManagedServerInfo> {
        let mut stopped = Vec::new();
        {
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
            for key in keys {
                if let Some(entry) = servers.remove(&key) {
                    entry.client.lock().await.shutdown().await;
                    stopped.push(entry.info);
                }
            }
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

/// Pure predicate extracted out of reap_idle for direct unit testing.
fn is_stale(idle_since: i64, now: i64, cutoff_ms: i64) -> bool {
    now - idle_since > cutoff_ms
}

pub fn socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".lsp-cli")
        .join("manager.sock")
}

type SharedManager = Arc<Manager>;

async fn list_handler(State(m): State<SharedManager>) -> Json<Vec<ManagedServerInfo>> {
    Json(m.list().await)
}

async fn create_handler(
    State(m): State<SharedManager>,
    Json(req): Json<CreateRequest>,
) -> Result<Json<ManagedServerInfo>, (axum::http::StatusCode, String)> {
    m.create(&req.path)
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
) -> Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    m.proxy_notify(
        &req.project_root,
        req.language.as_deref(),
        &req.method,
        req.params,
    )
    .await
    .map(|_| axum::http::StatusCode::NO_CONTENT)
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn shutdown_handler(State(m): State<SharedManager>) -> axum::http::StatusCode {
    let servers = m.servers.lock().await;
    for s in servers.values() {
        s.client.lock().await.shutdown().await;
    }
    drop(servers);
    m.watcher.dispose().await;
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::process::exit(0);
    });
    axum::http::StatusCode::NO_CONTENT
}

pub fn app(manager: SharedManager) -> Router {
    Router::new()
        .route("/list", get(list_handler))
        .route("/create", post(create_handler))
        .route("/delete", delete(delete_handler))
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
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let (manager, mut watch_rx) = Manager::new();
    let manager: SharedManager = Arc::new(manager);

    let idle_manager = manager.clone();
    let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            idle_manager.reap_idle(idle_timeout).await;
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
    tokio::select! {
        res = serve_uds(listener, router) => { res?; }
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

async fn serve_uds(listener: tokio::net::UnixListener, router: Router) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let router = router.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |req| {
                let router = router.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(
                        tower::ServiceExt::oneshot(router, req).await.unwrap(),
                    )
                }
            });
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, service)
                    .await;
        });
    }
}
