//! End-to-end test for the daemon's file watcher (src/watcher.rs): starts
//! the daemon directly (so its stderr is observable — `ensure_running`
//! normally spawns it with stderr discarded), starts a real server for the
//! TypeScript fixture, edits a watched file the server never opened, and
//! asserts the watcher's `[watcher] ... change(s) detected` log line
//! appears — proof the notify-based watch, debounce, and
//! `workspace/didChangeWatchedFiles` broadcast actually fired.
mod support;

use std::io::BufReader;
use std::process::{ChildStderr, Command, Stdio};
use support::{has_ts_server, lsp, ts_fixture};

#[test]
fn editing_an_unopened_file_triggers_a_watch_notification() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }

    let _ = lsp(&["server", "shutdown"]);
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lsp"))
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn daemon directly");
    let stderr = BufReader::new(daemon.stderr.take().unwrap());

    // Give the daemon a moment to bind its socket before anything tries to
    // talk to it (server start below also tolerates this via ensure_running's
    // own retry/wait, but this keeps the test's own timing predictable).
    std::thread::sleep(std::time::Duration::from_millis(300));

    let file = ts_fixture("src/models.ts");
    let start = lsp(&["server", "start", file.to_str().unwrap()]);
    assert_eq!(start.exit_code, 0, "{}", start.stderr);

    // Edit a *different* file in the same project — one the server was
    // never told to open via textDocument/didOpen — to prove the watcher
    // (not just didOpen-triggered indexing) is what's picking this up.
    let watched_file = ts_fixture("src/index.ts");
    let original = std::fs::read_to_string(&watched_file).unwrap();
    std::fs::write(&watched_file, format!("{original}// watcher-test-edit\n")).unwrap();

    let found = read_stderr_until(stderr, "[watcher]", std::time::Duration::from_secs(5));

    // Always restore the fixture, even if the assertion below fails.
    std::fs::write(&watched_file, &original).unwrap();

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = lsp(&["server", "shutdown"]);

    assert!(found, "expected a `[watcher] ... change(s) detected` line on the daemon's stderr within 5s");
}

fn read_stderr_until(mut reader: BufReader<ChildStderr>, needle: &str, timeout: std::time::Duration) -> bool {
    // BufRead::read_line blocks, so run the read loop on its own thread
    // (BufReader<ChildStderr> is Send) and race it against the timeout
    // instead of blocking the test forever if the watcher never fires.
    let (tx, rx) = std::sync::mpsc::channel();
    let needle = needle.to_string();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::BufRead::read_line(&mut reader, &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.contains(&needle) {
                        let _ = tx.send(true);
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(false);
    });
    rx.recv_timeout(timeout).unwrap_or(false)
}
