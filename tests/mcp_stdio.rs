mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn send(stdin: &mut impl Write, req: serde_json::Value) {
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();
}

fn recv(reader: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("invalid JSON-RPC line: {e}\nline: {line}"))
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
}
