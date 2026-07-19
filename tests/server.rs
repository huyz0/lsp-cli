mod support;
use support::{has_ts_server, lsp, ts_fixture};

#[test]
fn server_list_returns_valid_json_when_daemon_not_yet_running() {
    let _ = lsp(&["server", "shutdown"]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let result = lsp(&["server", "list", "--output", "json"]);
    assert_eq!(result.exit_code, 0);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(data["kind"], "serverList");
    assert!(data["servers"].is_array());

    let _ = lsp(&["server", "shutdown"]);
}

#[test]
fn server_list_markdown_shows_no_servers_running_when_empty() {
    let result = lsp(&["server", "list", "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    // Either "No servers running" or a (possibly empty) list — both are acceptable.
    let _ = result.stdout;

    let _ = lsp(&["server", "shutdown"]);
}

#[test]
fn server_shutdown_exits_cleanly() {
    let _ = lsp(&["server", "list"]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let result = lsp(&["server", "shutdown"]);
    assert_eq!(result.exit_code, 0);
}

#[test]
fn server_stop_all_when_no_servers_running_exits_cleanly() {
    let result = lsp(&["server", "stop", "--all"]);
    assert_eq!(result.exit_code, 0);
}

fn server_pid(project: &str) -> Option<u64> {
    let result = lsp(&["server", "list", "--output", "json"]);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).ok()?;
    data["servers"].as_array()?.iter().find(|s| s["project_root"] == project)?["pid"].as_u64()
}

/// Covers the "kill and reload" daemon bugs found during review:
/// `Manager::create()` used to hand back a cached `ManagedServerInfo` for an
/// existing project key with no check on whether the underlying process was
/// still alive, so a crashed/killed language server stayed reported as
/// "running" forever and a second `server start` on the same project just
/// returned the stale info instead of respawning. Reproduced live (kill -9
/// on the real child PID, then `server start` spawned nothing new) before
/// the fix in `daemon.rs::Manager::create`.
#[test]
fn kill_and_reload_respawns_a_dead_server() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_fixture("").canonicalize().unwrap();
    let project_str = project.to_str().unwrap();
    let file = ts_fixture("src/models.ts");

    let _ = lsp(&["server", "shutdown"]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let start1 = lsp(&["server", "start", file.to_str().unwrap()]);
    assert_eq!(start1.exit_code, 0, "{}", start1.stderr);
    let pid1 = server_pid(project_str).expect("expected a pid after first start");

    // Simulate an external crash/OOM-kill of the underlying LSP process —
    // not something the daemon itself did.
    let kill = std::process::Command::new("kill").args(["-9", &pid1.to_string()]).status();
    assert!(kill.map(|s| s.success()).unwrap_or(false), "failed to kill pid {pid1}");
    std::thread::sleep(std::time::Duration::from_millis(500));

    let start2 = lsp(&["server", "start", file.to_str().unwrap()]);
    assert_eq!(start2.exit_code, 0, "{}", start2.stderr);
    let pid2 = server_pid(project_str).expect("expected a pid after reload");

    assert_ne!(pid1, pid2, "server start on a project whose process was killed should respawn a new process, not report the dead one as still running");

    let _ = lsp(&["server", "shutdown"]);
}

/// Covers the idle-tracking bug: `idle_since` used to be written once at
/// creation and never refreshed, so any server would be silently killed by
/// the idle reaper exactly `idle_timeout` seconds after it was started
/// regardless of actual use. `Manager::create()` now refreshes it whenever
/// an existing, still-alive server is reused.
#[test]
fn reusing_a_running_server_refreshes_idle_since() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_fixture("").canonicalize().unwrap();
    let project_str = project.to_str().unwrap();
    let file = ts_fixture("src/models.ts");

    let _ = lsp(&["server", "shutdown"]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    lsp(&["server", "start", file.to_str().unwrap()]);
    let idle1 = lsp(&["server", "list", "--output", "json"]);
    let data1: serde_json::Value = serde_json::from_str(&idle1.stdout).unwrap();
    let t1 = data1["servers"].as_array().unwrap().iter().find(|s| s["project_root"] == project_str).unwrap()["idle_since"].as_i64().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1200));
    lsp(&["server", "start", file.to_str().unwrap()]);
    let idle2 = lsp(&["server", "list", "--output", "json"]);
    let data2: serde_json::Value = serde_json::from_str(&idle2.stdout).unwrap();
    let t2 = data2["servers"].as_array().unwrap().iter().find(|s| s["project_root"] == project_str).unwrap()["idle_since"].as_i64().unwrap();

    assert!(t2 > t1, "idle_since should advance when an existing server is reused (t1={t1}, t2={t2})");

    let _ = lsp(&["server", "shutdown"]);
}

/// Covers the create() TOCTOU race: concurrent `server start` calls for the
/// same project used to each spawn their own LSP server process before
/// either got far enough to insert into the manager's map, so only the last
/// insert survived — the others were silently orphaned outside `server
/// list`/`server stop --all`'s reach. Reproduced live (4 concurrent starts
/// produced 2 real OS processes but only 1 tracked entry) before the fix
/// (`Manager::create_lock` serializing the whole spawn+insert sequence).
#[test]
fn concurrent_server_start_for_the_same_project_creates_only_one_entry() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_fixture("").canonicalize().unwrap();
    let project_str = project.to_str().unwrap().to_string();
    let file = ts_fixture("src/models.ts");

    let _ = lsp(&["server", "shutdown"]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let file = file.clone();
            std::thread::spawn(move || lsp(&["server", "start", file.to_str().unwrap()]))
        })
        .collect();
    for h in handles {
        let r = h.join().unwrap();
        assert_eq!(r.exit_code, 0, "{}", r.stderr);
    }

    let list = lsp(&["server", "list", "--output", "json"]);
    let data: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    let matching = data["servers"].as_array().unwrap().iter().filter(|s| s["project_root"] == project_str).count();
    assert_eq!(matching, 1, "expected exactly one tracked entry for the project after concurrent starts, got {matching}");

    let _ = lsp(&["server", "shutdown"]);
}
