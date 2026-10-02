//! Minimal MCP (Model Context Protocol) server over stdio, matching the tool
//! surface of commands/mcp.ts: one MCP tool per navigation subcommand, each
//! implemented by re-invoking this same binary as a subprocess with `--json`
//! and capturing its stdout — exactly like the TS version does via
//! `Bun.spawnSync`. Only the stdio transport is implemented (the TS SSE/HTTP
//! transport is not ported).

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

use crate::schema::schemas;

pub fn run_mcp_stdio(project: Option<&str>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // JSON-RPC 2.0: an unparseable message gets a Parse error
                // with a null id, not silence — a client waiting on that
                // request would otherwise wait forever.
                let response = json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                writeln!(stdout, "{response}")?;
                stdout.flush()?;
                continue;
            }
        };

        // A message without an id is a notification (`notifications/
        // initialized`, `notifications/cancelled`, ...). JSON-RPC forbids
        // answering one; this used to reply "Unknown method" with a null
        // id, which strict clients reject as a protocol violation.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "lsp-cli", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": {} }
                }
            }),
            "tools/list" => {
                let tools: Vec<Value> = schemas()
                    .into_iter()
                    .map(|(name, schema)| {
                        json!({
                            "name": name,
                            "description": schema.get("description").cloned().unwrap_or(json!("")),
                            "inputSchema": {
                                "type": "object",
                                "properties": schema.get("properties").cloned().unwrap_or(json!({})),
                                "required": schema.get("required").cloned().unwrap_or(json!([])),
                            }
                        })
                    })
                    .collect();
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
            }
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(json!({}));
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));

                let all = schemas();
                if !all.contains_key(name) {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Unknown tool: {name}") } })
                } else {
                    let mut cmd = std::process::Command::new(&exe);
                    cmd.arg(name).arg("--json").arg(args.to_string());
                    // Only for tools that actually accept it. `--project`
                    // used to be appended to every invocation, so running
                    // `lsp mcp --project <p>` made the locate, install and
                    // server tools fail with a clap usage error on every
                    // call — they have no such flag.
                    let accepts_project = all
                        .get(name)
                        .and_then(|schema| schema.get("properties"))
                        .and_then(|props| props.get("project"))
                        .is_some();
                    if let (Some(p), true) = (project, accepts_project) {
                        cmd.arg("--project").arg(p);
                    }
                    let output = cmd.output();
                    match output {
                        Ok(out) => {
                            let is_error = !out.status.success();
                            let stdout_text = String::from_utf8_lossy(&out.stdout).to_string();
                            let stderr_text = String::from_utf8_lossy(&out.stderr).to_string();
                            let mut content = vec![];
                            if is_error {
                                content.push(json!({ "type": "text", "text": stderr_text }));
                            } else {
                                content.push(json!({ "type": "text", "text": stdout_text }));
                                // Notices a shell user sees on stderr — "N
                                // more results, use --start-index" above
                                // all — were dropped entirely here, so an
                                // agent over MCP never learned a page was
                                // truncated. Kept as a separate item so the
                                // first one is still exactly the result.
                                if !stderr_text.trim().is_empty() {
                                    content.push(json!({ "type": "text", "text": stderr_text }));
                                }
                            }
                            json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "isError": is_error, "content": content }
                            })
                        }
                        Err(e) => {
                            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e.to_string() } })
                        }
                    }
                }
            }
            other => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Unknown method: {other}") } })
            }
        };

        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
