//! CLI-side client that talks to the background manager daemon over its Unix
//! Domain Socket using a minimal hand-rolled HTTP/1.1 client (the daemon
//! speaks real HTTP via axum/hyper, so this just needs to be a correct
//! client, not a framework). Mirrors manager/client.ts.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::daemon::{socket_path, ManagedServerInfo};

pub struct ManagerClient;

fn spawn_lock_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".lsp-cli").join("manager.spawn.lock")
}

/// A lock file older than this is assumed to belong to a spawner that
/// crashed (or was killed) before removing it, rather than one still
/// legitimately in progress — `ensure_running` normally spawns and confirms
/// liveness within its own 8s deadline, so anything older than that is stale.
const SPAWN_LOCK_STALE_AFTER: Duration = Duration::from_secs(15);

fn spawn_lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| mtime.elapsed().map(|age| age > SPAWN_LOCK_STALE_AFTER).unwrap_or(true))
        .unwrap_or(true) // can't stat it (e.g. already gone) — safe to treat as not blocking
}

impl ManagerClient {
    pub fn new() -> Self {
        Self
    }

    pub async fn is_alive(&self) -> bool {
        raw_request("GET", "/list", None).await.is_ok()
    }

    /// Starts the background daemon if none is running, serialized across
    /// concurrent processes via an atomically-created lock file.
    ///
    /// A plain "check is_alive, then spawn" (the previous implementation) is
    /// a TOCTOU race: two CLI invocations racing with no daemon up can both
    /// observe "not alive" and both spawn `lsp --daemon`, and since
    /// `start_daemon` used to unconditionally delete-then-bind the socket
    /// path, the second daemon's bind would delete the first daemon's live
    /// socket out from under it — orphaning it permanently (it keeps
    /// running, listening on an unlinked inode, unreachable and unkillable
    /// via `lsp server shutdown`). Reproduced live during review: 5
    /// concurrent invocations against a cold socket left 2 daemons alive at
    /// once. `std::fs::OpenOptions::create_new` is atomic (`O_EXCL`) even
    /// across processes sharing a filesystem, so exactly one process wins
    /// the right to spawn; everyone else waits on `is_alive()` instead of
    /// also spawning. `start_daemon` also independently connect-checks the
    /// socket before touching it, as defense in depth.
    pub async fn ensure_running(&self) -> Result<()> {
        if self.is_alive().await {
            return Ok(());
        }

        let lock_path = spawn_lock_path();
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let acquired = loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    break true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if spawn_lock_is_stale(&lock_path) {
                        // Assume the previous holder crashed before cleaning
                        // up; take over rather than waiting forever.
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    break false;
                }
                Err(e) => bail!("failed to create daemon spawn lock {}: {e}", lock_path.display()),
            }
        };

        if !acquired {
            // Someone else is already spawning — wait for their daemon
            // instead of also spawning one ourselves.
            return self.wait_for_alive().await;
        }

        let result = self.spawn_daemon_and_wait().await;
        let _ = std::fs::remove_file(&lock_path);
        result
    }

    async fn spawn_daemon_and_wait(&self) -> Result<()> {
        // Re-check: another process's daemon may have become alive while we
        // were acquiring the lock.
        if self.is_alive().await {
            return Ok(());
        }

        let exe = std::env::current_exe()?;
        std::process::Command::new(exe)
            .arg("--daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn daemon: {e}"))?;

        self.wait_for_alive().await
    }

    async fn wait_for_alive(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.is_alive().await {
                return Ok(());
            }
        }
        bail!("lsp-cli daemon failed to start within 8s. Check ~/.lsp-cli/logs/ for errors.");
    }

    pub async fn list_servers(&self) -> Result<Vec<ManagedServerInfo>> {
        let (_, body) = raw_request("GET", "/list", None).await?;
        Ok(serde_json::from_str(&body)?)
    }

    pub async fn create_server(&self, path: &str) -> Result<ManagedServerInfo> {
        let body = serde_json::json!({ "path": path }).to_string();
        let (status, resp) = raw_request("POST", "/create", Some(body)).await?;
        if status != 200 {
            bail!("{resp}");
        }
        Ok(serde_json::from_str(&resp)?)
    }

    pub async fn delete_servers(&self, path: Option<&str>, all: bool) -> Result<Vec<ManagedServerInfo>> {
        let body = serde_json::json!({ "path": path, "all": all }).to_string();
        let (_, resp) = raw_request("DELETE", "/delete", Some(body)).await?;
        Ok(serde_json::from_str(&resp)?)
    }

    /// Sends an LSP request to the warm, daemon-managed server for
    /// `project_root` and returns its result — used by the navigation
    /// commands instead of spawning their own one-shot `LspClient`.
    pub async fn proxy_request(&self, project_root: &str, language: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let body = serde_json::json!({ "project_root": project_root, "language": language, "method": method, "params": params }).to_string();
        let (status, resp) = raw_request("POST", "/request", Some(body)).await?;
        if status != 200 {
            bail!("{resp}");
        }
        Ok(serde_json::from_str(&resp)?)
    }

    /// Same as `proxy_request` but for a notification with no result.
    pub async fn proxy_notify(&self, project_root: &str, language: Option<&str>, method: &str, params: Value) -> Result<()> {
        let body = serde_json::json!({ "project_root": project_root, "language": language, "method": method, "params": params }).to_string();
        let (status, resp) = raw_request("POST", "/notify", Some(body)).await?;
        if status != 204 {
            bail!("{resp}");
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _ = raw_request("POST", "/shutdown", Some("{}".into())).await;
        Ok(())
    }
}

/// Extra attempts `raw_request` makes when the connection itself fails
/// (refused/reset while dialing or mid-response) rather than when the
/// daemon returns a real HTTP error status — the latter is a genuine
/// failure and is never retried here. Reproduced live: running the full
/// integration-test suite with default (parallel) `cargo test` — dozens of
/// real language-server child processes (rust-analyzer, jdtls, tsserver,
/// etc.) spawning and running concurrently starve the daemon process of
/// CPU/scheduling long enough that some of the many simultaneous Unix
/// socket connections its `hyper` listener is mid-accepting get dropped,
/// surfacing to this client as `Connection reset by peer` or a
/// zero/partial read that fails to parse as an HTTP status line. Each
/// dropped connection here means the daemon's own request handler for it
/// never ran (or didn't run to completion) — connect/write/read all
/// failing before a well-formed response was ever produced is exactly the
/// class of failure safe to retry: nothing server-side executed exactly
/// once and can't be safely reissued (unlike, say, a partially-applied
/// side effect).
const MAX_TRANSPORT_RETRIES: u32 = 4;
const TRANSPORT_RETRY_BACKOFF_MS: u64 = 150;

async fn raw_request(method: &str, path: &str, body: Option<String>) -> Result<(u16, String)> {
    let mut attempt = 0;
    loop {
        match raw_request_once(method, path, body.as_deref()).await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_TRANSPORT_RETRIES => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(TRANSPORT_RETRY_BACKOFF_MS * attempt as u64)).await;
                let _ = e; // transient — retried below
            }
            Err(e) => return Err(e),
        }
    }
}

async fn raw_request_once(method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock).await.map_err(|e| anyhow!("cannot reach manager daemon: {e}"))?;

    let body = body.unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);

    stream.write_all(req.as_bytes()).await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw);

    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let resp_body = parts.next().unwrap_or("").to_string();

    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP response from daemon"))?;

    Ok((status, resp_body))
}
