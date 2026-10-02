//! Daemon lifecycle tests.
//!
//! Every test here gets its own `LSP_CLI_HOME`, and therefore its own
//! daemon and socket. They used to share the developer's real daemon, so
//! `server shutdown` in one test killed the daemon another was mid-way
//! through using — reliably failing 3 of 7 with "cannot reach manager
//! daemon: Connection refused" when cargo ran them in parallel. Isolation
//! is what makes them deterministic; it also means running the suite no
//! longer terminates whatever servers the developer had warm.

mod support;
use support::{has_ts_server, isolated_home, lsp_in, ts_fixture, IsolatedHome};

#[test]
fn server_list_returns_valid_json_when_daemon_not_yet_running() {
    let home = isolated_home("list-cold");

    let result = lsp_in(&home, &["server", "list", "--output", "json"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(data["kind"], "serverList");
    assert_eq!(
        data["servers"].as_array().map(|a| a.len()),
        Some(0),
        "a fresh state directory has no servers"
    );
}

#[test]
fn server_list_markdown_says_so_when_nothing_is_running() {
    let home = isolated_home("list-md");
    let result = lsp_in(&home, &["server", "list", "--output", "markdown"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    // Previously this asserted nothing at all ("either output is
    // acceptable"), leaving the markdown formatter untested.
    assert!(
        result.stdout.contains("No servers running"),
        "expected the empty-list message, got: {:?}",
        result.stdout
    );
}

#[test]
fn server_shutdown_exits_cleanly_and_removes_the_socket() {
    let home = isolated_home("shutdown");
    // Force a daemon to exist.
    assert_eq!(lsp_in(&home, &["server", "list"]).exit_code, 0);
    let socket = home.path().join("manager.sock");
    assert!(socket.exists(), "daemon should have bound its socket");

    let result = lsp_in(&home, &["server", "shutdown"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);

    // `process::exit` used to skip the socket cleanup, leaving a dead
    // socket that made every later connect fail with ECONNREFUSED rather
    // than the ENOENT that means "no daemon".
    wait_until(|| !socket.exists(), "socket removed on shutdown");
}

#[test]
fn server_stop_all_when_no_servers_running_exits_cleanly() {
    let home = isolated_home("stop-all-empty");
    // JSON is the default output format, so assert on that shape; the
    // prose form is the markdown branch.
    let result = lsp_in(&home, &["server", "stop", "--all"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(data["kind"], "serversStopped");
    assert_eq!(data["servers"].as_array().map(|a| a.len()), Some(0));

    let md = lsp_in(&home, &["server", "stop", "--all", "--output", "markdown"]);
    assert_eq!(md.exit_code, 0, "{}", md.stderr);
    assert!(md.stdout.contains("No servers stopped"));
}

fn server_pid(home: &IsolatedHome, project: &str) -> Option<u64> {
    let result = lsp_in(home, &["server", "list", "--output", "json"]);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).ok()?;
    data["servers"]
        .as_array()?
        .iter()
        .find(|s| s["project_root"] == project)?["pid"]
        .as_u64()
}

/// Polls `cond` for up to 5s. Replaces the fixed sleeps these tests used
/// to rely on, which made them load-sensitive.
fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

/// Covers the "kill and reload" daemon bug: `Manager::create()` used to
/// hand back a cached `ManagedServerInfo` for an existing project key with
/// no check on whether the underlying process was still alive, so a
/// crashed/killed language server stayed reported as "running" forever and
/// a second `server start` returned the stale info instead of respawning.
#[test]
fn kill_and_reload_respawns_a_dead_server() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let home = isolated_home("kill-reload");
    let project = ts_fixture("").canonicalize().unwrap();
    let project_str = project.to_str().unwrap();
    let file = ts_fixture("src/models.ts");

    let start1 = lsp_in(&home, &["server", "start", file.to_str().unwrap()]);
    assert_eq!(start1.exit_code, 0, "{}", start1.stderr);
    let pid1 = server_pid(&home, project_str).expect("expected a pid after first start");

    // Simulate an external crash/OOM-kill of the underlying LSP process —
    // not something the daemon itself did.
    let kill = std::process::Command::new("kill")
        .args(["-9", &pid1.to_string()])
        .status();
    assert!(
        kill.map(|s| s.success()).unwrap_or(false),
        "failed to kill pid {pid1}"
    );
    // The killed process stays a zombie until the daemon reaps it, so its
    // /proc entry lingers — poll the behaviour we actually care about
    // instead: `server start` eventually reports a different pid, because
    // `Manager::create`'s liveness check evicts the dead entry and respawns.
    let mut pid2 = pid1;
    wait_until(
        || {
            let start2 = lsp_in(&home, &["server", "start", file.to_str().unwrap()]);
            if start2.exit_code != 0 {
                return false;
            }
            match server_pid(&home, project_str) {
                Some(p) => {
                    pid2 = p;
                    p != pid1
                }
                None => false,
            }
        },
        "a killed server to be respawned with a new pid",
    );

    assert_ne!(pid1, pid2, "server start on a project whose process was killed should respawn a new process, not report the dead one as still running");
}

/// Covers the idle-tracking bug: `idle_since` used to be written once at
/// creation and never refreshed, so any server would be silently killed by
/// the idle reaper exactly `idleTimeout` seconds after it was started
/// regardless of actual use.
#[test]
fn reusing_a_running_server_refreshes_idle_since() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let home = isolated_home("idle-refresh");
    let project = ts_fixture("").canonicalize().unwrap();
    let project_str = project.to_str().unwrap();
    let file = ts_fixture("src/models.ts");

    let idle_since = |h: &IsolatedHome| -> i64 {
        let out = lsp_in(h, &["server", "list", "--output", "json"]);
        let data: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        data["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["project_root"] == project_str)
            .unwrap_or_else(|| panic!("no server for {project_str} in {}", out.stdout))
            ["idle_since"]
            .as_i64()
            .unwrap()
    };

    assert_eq!(
        lsp_in(&home, &["server", "start", file.to_str().unwrap()]).exit_code,
        0
    );
    let t1 = idle_since(&home);

    // `idle_since` is a millisecond timestamp, so a short wait is enough to
    // make a refresh observable.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        lsp_in(&home, &["server", "start", file.to_str().unwrap()]).exit_code,
        0
    );
    let t2 = idle_since(&home);

    assert!(
        t2 > t1,
        "idle_since should advance when an existing server is reused (t1={t1}, t2={t2})"
    );
}

/// Covers the create() TOCTOU race: concurrent `server start` calls for the
/// same project used to each spawn their own LSP server process before
/// either got far enough to insert into the manager's map, so only the last
/// insert survived and the others were silently orphaned.
#[test]
fn concurrent_server_start_for_the_same_project_creates_only_one_entry() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let home = std::sync::Arc::new(isolated_home("concurrent-start"));
    let project = ts_fixture("").canonicalize().unwrap();
    let project_str = project.to_str().unwrap().to_string();
    let file = ts_fixture("src/models.ts");

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let file = file.clone();
            let home = home.clone();
            std::thread::spawn(move || lsp_in(&home, &["server", "start", file.to_str().unwrap()]))
        })
        .collect();
    for h in handles {
        let r = h.join().unwrap();
        assert_eq!(r.exit_code, 0, "{}", r.stderr);
    }

    let list = lsp_in(&home, &["server", "list", "--output", "json"]);
    let data: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    let matching = data["servers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["project_root"] == project_str)
        .count();
    assert_eq!(matching, 1, "expected exactly one tracked entry for the project after concurrent starts, got {matching}");
}

/// Ctrl-C on the command that happened to spawn the daemon (or on an agent
/// driving it) must not take the daemon and every warm server down with
/// it. The daemon used to inherit the spawning CLI's process group, so a
/// SIGINT to that group reached it too.
#[test]
fn sigint_to_the_spawning_command_s_process_group_does_not_kill_the_daemon() {
    use std::os::unix::process::CommandExt;
    let home = isolated_home("sigint");
    let mut cli = std::process::Command::new(support::bin_path());
    cli.args(["server", "list"])
        .env("LSP_CLI_HOME", home.path())
        .stdout(std::process::Stdio::null())
        .process_group(0);
    let child = cli.spawn().unwrap();
    let pgid = child.id();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let socket = home.path().join("manager.sock");
    assert!(socket.exists(), "the command should have started a daemon");

    // Whatever is still in the spawning command's group gets the SIGINT a
    // terminal Ctrl-C would deliver. `kill` failing (no such group) is the
    // expected outcome when the daemon isn't in it.
    // POSIX form via `sh`, so it means "process group" on both Linux and
    // macOS kill implementations.
    let _ = std::process::Command::new("sh")
        .args(["-c", &format!("kill -s INT -- -{pgid}")])
        .stderr(std::process::Stdio::null())
        .status();
    std::thread::sleep(std::time::Duration::from_millis(500));

    assert!(
        socket.exists(),
        "the daemon shut down on SIGINT to its parent's group"
    );
    let result = lsp_in(&home, &["server", "list", "--output", "json"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
}

/// A request that is slow to answer holds its server's client lock for as
/// long as it runs. `create` used to wait for that lock while holding the
/// daemon-wide server map, so one slow hover stalled every other call in
/// the daemon — including `server list`, which is also the CLI's liveness
/// probe.
#[test]
fn a_slow_request_does_not_stall_unrelated_daemon_calls() {
    let project = support::FakeServerProject::new(
        "slow-request",
        "fn first() {}\nfn second() {}\n",
        &[("FAKE_LSP_HOVER_DELAY", "8")],
    );
    let file = project.file();
    let warm = project.run(&["outline", &file, "--output", "json"]);
    assert_eq!(warm.exit_code, 0, "{}", warm.stderr);

    std::thread::scope(|s| {
        // Hover on line 2 is the slow one.
        let slow = s.spawn(|| project.run(&["doc", &file, "--scope", "2"]));
        wait_until_secs(
            || project.markers.join("hover-started").exists(),
            "the slow hover to begin",
            30,
        );
        // A second command for the same project reaches `create` while the
        // hover holds the client lock...
        let second = s.spawn(|| project.run(&["outline", &file, "--output", "json"]));
        std::thread::sleep(std::time::Duration::from_millis(300));
        // ...and an unrelated daemon call must still answer promptly.
        let started = std::time::Instant::now();
        let list = project.run(&["server", "list", "--output", "json"]);
        let took = started.elapsed();
        assert_eq!(list.exit_code, 0, "{}", list.stderr);
        assert!(
            took < std::time::Duration::from_secs(3),
            "`server list` took {took:?} while a slow request was in flight"
        );
        let slow = slow.join().unwrap();
        assert_eq!(slow.exit_code, 0, "{}", slow.stderr);
        assert!(
            slow.stdout.contains("hover: fn second() {}"),
            "{}",
            slow.stdout
        );
        assert_eq!(second.join().unwrap().exit_code, 0);
    });
}

fn wait_until_secs(mut cond: impl FnMut() -> bool, what: &str, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

/// A command issued right after `server shutdown` used to find the old
/// daemon still bound to the socket, pass its liveness check, and then fail
/// with "cannot reach manager daemon" once that daemon went away, instead
/// of starting a new one.
#[test]
fn a_command_right_after_shutdown_starts_a_fresh_daemon() {
    let home = isolated_home("after-shutdown");
    assert_eq!(lsp_in(&home, &["server", "list"]).exit_code, 0);
    for _ in 0..3 {
        let down = lsp_in(&home, &["server", "shutdown"]);
        assert_eq!(down.exit_code, 0, "{}", down.stderr);
        let next = lsp_in(&home, &["server", "list", "--output", "json"]);
        assert_eq!(next.exit_code, 0, "{}", next.stderr);
    }
}
