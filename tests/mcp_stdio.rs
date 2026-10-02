mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn send(stdin: &mut impl Write, req: serde_json::Value) {
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();
}

/// Reads one JSON-RPC line, giving up rather than blocking forever.
///
/// This was a bare `read_line`, which meant a server that deadlocked or
/// exited mid-protocol hung the test until the CI job timed out instead of
/// failing — and these are among the few tests CI actually runs. The read
/// happens on a worker thread so the timeout can win the race.
fn recv(reader: &mut (impl BufRead + Send)) -> serde_json::Value {
    let line = read_line_with_timeout(reader, std::time::Duration::from_secs(20));
    serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("invalid JSON-RPC line: {e}\nline: {line}"))
}

fn read_line_with_timeout(
    reader: &mut (impl BufRead + Send),
    timeout: std::time::Duration,
) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = tx.send(line);
        });
        rx.recv_timeout(timeout)
            .expect("timed out waiting for a JSON-RPC response from `lsp mcp`")
    })
}

#[test]
fn mcp_server_lists_tools_over_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lsp"))
        .args(["mcp", "--transport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    );
    let init = recv(&mut reader);
    assert_eq!(init["result"]["serverInfo"]["name"], "lsp-cli");

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let list = recv(&mut reader);
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    assert!(tools.iter().any(|t| t["name"] == "outline"));

    let _ = child.kill();
    let _ = child.wait(); // reap it; a bare kill() leaves a zombie
}

#[test]
fn mcp_server_executes_tool_over_stdio() {
    let models = support::ts_fixture("src/models.ts");

    let mut child = Command::new(env!("CARGO_BIN_EXE_lsp"))
        .args(["mcp", "--transport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    );
    recv(&mut reader);

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "locate", "arguments": {"file": models.to_str().unwrap(), "scope": "User"}}
        }),
    );
    let call = recv(&mut reader);
    assert_eq!(call["result"]["isError"], false);
    let content = call["result"]["content"].as_array().unwrap();
    assert!(!content.is_empty());
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().contains("User"));

    let _ = child.kill();
    let _ = child.wait(); // reap it; a bare kill() leaves a zombie
}

#[test]
fn mcp_server_reports_unknown_tool() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lsp"))
        .args(["mcp", "--transport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "not-a-real-tool", "arguments": {}}}),
    );
    let resp = recv(&mut reader);
    assert!(resp.get("error").is_some());

    let _ = child.kill();
    let _ = child.wait(); // reap it; a bare kill() leaves a zombie
}

/// JSON-RPC rules a strict MCP client relies on: notifications are never
/// answered, `ping` is, and an unparseable line gets a Parse error rather
/// than silence.
#[test]
fn mcp_follows_json_rpc_notification_ping_and_parse_error_rules() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lsp"))
        .args(["mcp", "--transport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    );
    assert_eq!(recv(&mut reader)["id"], 1);

    // Two notifications, then a request: the very next line must be the
    // request's answer. Any reply to a notification would arrive first.
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 9}}),
    );
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": "p-1", "method": "ping"}),
    );
    let ping = recv(&mut reader);
    assert_eq!(ping["id"], "p-1", "got a reply to a notification: {ping}");
    assert_eq!(ping["result"], serde_json::json!({}));

    writeln!(stdin, "{{ this is not json").unwrap();
    stdin.flush().unwrap();
    let parse_error = recv(&mut reader);
    assert_eq!(parse_error["error"]["code"], -32700, "{parse_error}");
    assert!(parse_error["id"].is_null());

    let _ = child.kill();
    let _ = child.wait();
}

/// The `install` tool always failed: the JSON-args bridge appended
/// `--output json`, which `install` doesn't accept.
#[test]
fn mcp_install_tool_runs_instead_of_failing_on_an_output_flag() {
    let home = support::empty_home("mcp-install");
    let fakebin = home.path().join("fakebin");
    std::fs::create_dir_all(&fakebin).unwrap();
    support::write_script(&fakebin, "zls", "echo 0.13.0");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lsp"))
        .args(["mcp", "--transport", "stdio"])
        .env("LSP_CLI_HOME", home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "install", "arguments": {"language": "zig"}}
        }),
    );
    let call = recv(&mut reader);
    assert_eq!(call["result"]["isError"], false, "{call}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("found on PATH"), "{text}");

    let _ = child.kill();
    let _ = child.wait();
}
