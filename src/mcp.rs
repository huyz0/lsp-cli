//! Minimal MCP (Model Context Protocol) server over stdio, matching the tool
//! surface of commands/mcp.ts: one MCP tool per navigation subcommand, each
//! implemented by re-invoking this same binary as a subprocess with `--json`
//! and capturing its stdout — exactly like the TS version does via
//! `Bun.spawnSync`. Only the stdio transport is implemented (the TS SSE/HTTP
//! transport is not ported — see README).

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
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "lsp-cli", "version": "0.1.0" },
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

                if !schemas().contains_key(name) {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Unknown tool: {name}") } })
                } else {
                    let mut cmd = std::process::Command::new(&exe);
                    cmd.arg(name).arg("--json").arg(args.to_string());
                    if let Some(p) = project {
                        cmd.arg("--project").arg(p);
                    }
                    let output = cmd.output();
                    match output {
                        Ok(out) => {
                            let is_error = !out.status.success();
                            let text = if is_error {
                                String::from_utf8_lossy(&out.stderr).to_string()
                            } else {
                                String::from_utf8_lossy(&out.stdout).to_string()
                            };
                            json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": { "isError": is_error, "content": [{ "type": "text", "text": text }] }
                            })
                        }
                        Err(e) => {
                            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e.to_string() } })
                        }
                    }
                }
            }
            "" => continue,
            other => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Unknown method: {other}") } })
            }
        };

        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
