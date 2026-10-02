//! CLI-side client that talks to the background manager daemon over its Unix
//! Domain Socket using a minimal hand-rolled HTTP/1.1 client (the daemon
//! speaks real HTTP via axum/hyper, so this just needs to be a correct
//! client, not a framework). Mirrors manager/client.ts.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::daemon::{socket_path, ManagedServerInfo};

/// Talks to the daemon. `fresh` records whether the session this client
/// set up (see `commands::ensure_daemon_session`) just started a server or
/// changed a document, i.e. whether the server may still be catching up.
#[derive(Default)]
pub struct ManagerClient {
    pub fresh: bool,
}

use crate::paths::spawn_lock_path;

/// A lock file older than this is assumed to belong to a spawner that
/// crashed (or was killed) before removing it, rather than one still
/// legitimately in progress — `ensure_running` normally spawns and confirms
/// liveness within its own 8s deadline, so anything older than that is stale.
const SPAWN_LOCK_STALE_AFTER: Duration = Duration::from_secs(15);

/// How often `wait_for_alive` re-probes the socket while a daemon starts.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn spawn_lock_is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| {
            mtime
                .elapsed()
                .map(|age| age > SPAWN_LOCK_STALE_AFTER)
                .unwrap_or(true)
        })
        .unwrap_or(true) // can't stat it (e.g. already gone) — safe to treat as not blocking
}

impl ManagerClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Liveness probe. Deliberately does *not* go through `raw_request`'s
    /// retry loop: "is a daemon listening?" is a question whose answer is
    /// legitimately "no", and retrying it burned the full 150+300+450+600ms
    /// backoff before returning false. That cost was paid three times on
    /// every cold navigation command (here, in `ensure_running`, and again
    /// in `spawn_daemon_and_wait`) — about 4.5s of pure sleeping before the
    /// daemon process was even forked — and it stretched
    /// `wait_for_alive`'s intended 100ms poll interval to ~1.6s.
    pub async fn is_alive(&self) -> bool {
        raw_request_once("GET", "/list", None).await.is_ok()
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
        match raw_exchange("GET", "/list", None).await {
            Ok((_, _, build))
                if !crate::paths::is_older_build(build.as_deref(), crate::paths::build_id()) =>
            {
                return Ok(())
            }
            // A daemon from an older build of `lsp` (before an upgrade or a
            // rebuild) would keep serving with its old code for as long as
            // it had servers to keep it alive. Replace it.
            Ok((_, _, build)) => {
                eprintln!(
                    "[lsp] restarting the background daemon: it is from an older build of lsp ({}, this is {})",
                    build.as_deref().unwrap_or("unknown"),
                    crate::paths::build_id()
                );
                // Conditional on the build we saw: with several commands
                // starting at once, a sibling may already have replaced the
                // old daemon, and an unconditional shutdown would kill the
                // new one under it.
                let body = serde_json::json!({ "if_build": build }).to_string();
                let _ = raw_request_once("POST", "/shutdown", Some(&body)).await;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while socket_path().exists() && std::time::Instant::now() < deadline {
                    tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
                }
            }
            Err(_) => {}
        }

        // Checked before spawning rather than after waiting: an unbindable
        // socket path makes the daemon exit immediately, which the wait
        // loop below can only report as a timeout.
        if let Err(e) = crate::paths::check_socket_path(&socket_path()) {
            bail!("cannot start the lsp-cli daemon: {e}");
        }

        let lock_path = spawn_lock_path();
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let acquired = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
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
                Err(e) => bail!(
                    "failed to create daemon spawn lock {}: {e}",
                    lock_path.display()
                ),
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

        // Keep the daemon's stderr, rather than discarding it. It is
        // spawned detached, so anything it says about failing to start had
        // nowhere to go — while the timeout message below told the user to
        // go and read a log that was never written.
        let log_path = crate::paths::daemon_log_path();
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path);

        let exe = std::env::current_exe()?;
        let mut command = std::process::Command::new(exe);
        // Its own process group, so it is not part of the job that spawned
        // it. Sharing the CLI's group meant Ctrl-C on whatever command
        // happened to start the daemon (or on the agent running it)
        // delivered SIGINT to the daemon too, and it shut down every warm
        // server with it.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command
            .arg("--daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(match log {
                Ok(f) => std::process::Stdio::from(f),
                Err(_) => std::process::Stdio::null(),
            })
            .spawn()
            .map_err(|e| anyhow!("failed to spawn daemon: {e}"))?;

        self.wait_for_alive().await
    }

    async fn wait_for_alive(&self) -> Result<()> {
        // `managerTimeout` from ~/.lsp-cli/config.json. It was parsed,
        // defaulted, documented as "daemon request timeout" and unit
        // tested — and then never read by anything, so setting it had no
        // effect. This is the wait it describes.
        let timeout = Duration::from_secs(crate::config::load_config().manager_timeout);
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
            if self.is_alive().await {
                return Ok(());
            }
        }
        bail!(
            "lsp-cli daemon failed to start within {}s (managerTimeout). Check {} for errors.",
            timeout.as_secs(),
            crate::paths::daemon_log_path().display()
        );
    }

    pub async fn list_servers(&self) -> Result<Vec<ManagedServerInfo>> {
        let (_, body) = raw_request("GET", "/list", None).await?;
        Ok(serde_json::from_str(&body)?)
    }

    /// `project_root` is the root the caller resolved (see
    /// `project::resolve_project`). Passing it keeps the daemon's registry
    /// key identical to the key later `proxy_request`/`proxy_notify` calls
    /// use; omitting it lets the daemon detect one itself, which is what
    /// `lsp server start <dir>` wants.
    pub async fn create_server(
        &self,
        path: &str,
        project_root: Option<&str>,
        server_path: Option<&std::path::Path>,
        language: Option<&str>,
    ) -> Result<ManagedServerInfo> {
        let body = serde_json::json!({
            "path": path,
            "project_root": project_root,
            "server_path": server_path.map(|p| p.to_string_lossy()),
            "language": language,
        })
        .to_string();
        let (status, resp) = raw_request("POST", "/create", Some(body)).await?;
        if status != 200 {
            bail!("{resp}");
        }
        Ok(serde_json::from_str(&resp)?)
    }

    /// BM25 fallback search against the daemon's cached index for
    /// `project_root`.
    ///
    /// Worth the round trip because the daemon keeps the index between
    /// invocations: building one reads every source file in the project,
    /// which the CLI process used to redo from scratch on every search and
    /// then throw away at exit.
    pub async fn search(
        &self,
        project_root: &str,
        query: &str,
    ) -> Result<Vec<crate::protocol::SymbolInformation>> {
        let body = serde_json::json!({ "project_root": project_root, "query": query }).to_string();
        let (status, resp) = raw_request("POST", "/search", Some(body)).await?;
        if status != 200 {
            bail!("{resp}");
        }
        Ok(serde_json::from_str(&resp)?)
    }

    pub async fn delete_servers(
        &self,
        path: Option<&str>,
        all: bool,
    ) -> Result<Vec<ManagedServerInfo>> {
        let body = serde_json::json!({ "path": path, "all": all }).to_string();
        let (_, resp) = raw_request("DELETE", "/delete", Some(body)).await?;
        Ok(serde_json::from_str(&resp)?)
    }

    /// Sends an LSP request to the warm, daemon-managed server for
    /// `project_root` and returns its result — used by the navigation
    /// commands instead of spawning their own one-shot `LspClient`.
    pub async fn proxy_request(
        &self,
        project_root: &str,
        language: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let body = serde_json::json!({ "project_root": project_root, "language": language, "method": method, "params": params }).to_string();
        let (status, resp) = raw_request("POST", "/request", Some(body)).await?;
        if status != 200 {
            if resp.starts_with("LSP error -32601") {
                return Err(anyhow!(Unsupported(unsupported_message(
                    language, method, &resp
                ))));
            }
            bail!("{resp}");
        }
        Ok(serde_json::from_str(&resp)?)
    }

    /// Same as `proxy_request` but for a notification with no result.
    pub async fn proxy_notify(
        &self,
        project_root: &str,
        language: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<bool> {
        let body = serde_json::json!({ "project_root": project_root, "language": language, "method": method, "params": params }).to_string();
        let (status, resp) = raw_request("POST", "/notify", Some(body)).await?;
        match status {
            200 => Ok(serde_json::from_str::<Value>(&resp)
                .ok()
                .and_then(|v| v.get("changed").and_then(|c| c.as_bool()))
                .unwrap_or(true)),
            // A daemon from an older build, which doesn't say: assume the
            // worst, i.e. that something changed.
            204 => Ok(true),
            _ => bail!("{resp}"),
        }
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
                tokio::time::sleep(std::time::Duration::from_millis(
                    TRANSPORT_RETRY_BACKOFF_MS * attempt as u64,
                ))
                .await;
                let _ = e; // transient — retried below
            }
            Err(e) => return Err(e),
        }
    }
}

async fn raw_request_once(method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    raw_exchange(method, path, body)
        .await
        .map(|(status, body, _)| (status, body))
}

/// One HTTP exchange with the daemon: status, body, and the daemon's
/// build id from the response headers (absent from daemons that predate
/// it).
async fn raw_exchange(
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, String, Option<String>)> {
    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| anyhow!("cannot reach manager daemon: {e}"))?;

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
    let build = head.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(crate::paths::BUILD_HEADER)
            .then(|| value.trim().to_string())
    });

    Ok((status, resp_body, build))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `spawn_lock_is_stale` guards the fix for a real bug the doc comment
    // above `ensure_running` describes — five concurrent invocations
    // against a cold socket left two daemons alive at once — and had no
    // regression test at all.

    #[test]
    fn a_freshly_created_lock_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager.spawn.lock");
        std::fs::write(&path, "12345").unwrap();
        assert!(
            !spawn_lock_is_stale(&path),
            "a lock written just now belongs to a live spawner"
        );
    }

    #[test]
    fn a_lock_older_than_the_threshold_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager.spawn.lock");
        std::fs::write(&path, "12345").unwrap();
        // Backdate well past SPAWN_LOCK_STALE_AFTER.
        let old = std::time::SystemTime::now() - SPAWN_LOCK_STALE_AFTER - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(
            spawn_lock_is_stale(&path),
            "a lock this old belongs to a spawner that died before cleaning up"
        );
    }

    #[test]
    fn a_missing_lock_is_treated_as_stale_rather_than_blocking() {
        // Can't stat it, so it isn't holding anything back. Treating this
        // as "still locked" would deadlock every future spawn attempt.
        let dir = tempfile::tempdir().unwrap();
        assert!(spawn_lock_is_stale(&dir.path().join("does-not-exist.lock")));
    }
}

/// A request the language server doesn't implement (JSON-RPC
/// MethodNotFound). Distinguished so callers don't retry it as if the
/// server were still warming up.
#[derive(Debug)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// "The python language server does not support textDocument/
/// prepareTypeHierarchy" rather than the raw `LSP error -32601: Unhandled
/// method ...`, plus what to use instead where there is an alternative.
fn unsupported_message(language: Option<&str>, method: &str, raw: &str) -> String {
    let server = language.map_or("language".to_string(), |l| format!("{l} language"));
    let hint = match method {
        "textDocument/prepareTypeHierarchy" | "typeHierarchy/supertypes" | "typeHierarchy/subtypes" => {
            "\nFor what implements an interface or extends a type, try `lsp reference <file> --scope <Type> --mode implementations`."
        }
        "textDocument/prepareCallHierarchy" => {
            "\nTo find callers, `lsp reference` lists every usage instead."
        }
        _ => "",
    };
    format!("The {server} server does not support `{method}` ({raw}).{hint}")
}

#[cfg(test)]
mod unsupported_tests {
    use super::*;

    #[test]
    fn method_not_found_is_explained_with_an_alternative() {
        let m = unsupported_message(
            Some("python"),
            "textDocument/prepareTypeHierarchy",
            "LSP error -32601: Unhandled method",
        );
        assert!(
            m.starts_with(
                "The python language server does not support `textDocument/prepareTypeHierarchy`"
            ),
            "{m}"
        );
        assert!(m.contains("--mode implementations"), "{m}");
        assert!(!unsupported_message(None, "textDocument/hover", "x").contains("\n"));
    }
}
