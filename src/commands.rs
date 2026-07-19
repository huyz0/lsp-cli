//! Command implementations. Each `run_*` function mirrors the corresponding
//! commands/*.ts file. Navigation commands (outline/definition/reference/
//! doc/symbol/search) proxy their LSP traffic through the background daemon
//! (`src/daemon.rs`) via `ensure_daemon_session`, so a language server
//! started for a project is reused warm across CLI invocations — including
//! across separate OS processes — instead of being spawned and killed fresh
//! on every single command. See README.md ("Reliability fixes" /
//! "Manager daemon") for the history here.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;

/// How long to wait after `didOpen`/`didChange` before issuing the actual
/// request, giving the server time to build its AST. Empirically tuned —
/// see the doc comment on `ensure_daemon_session` for the measurements
/// behind this number and README "Reliability fixes" for the full history.
const DIDOPEN_SETTLE_DELAY_MS: u64 = 3000;

use crate::bm25::Bm25Index;
use crate::format::OutputFormat;
use crate::locate::resolve_locate;
use crate::manager_client::ManagerClient;
use crate::project::{language_id, resolve_project, ProjectContext};
use crate::protocol::{
    symbol_kind_name, CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall,
    DocumentDiagnosticReport, DocumentSymbol, HoverResult, Location, LocationOrMany,
    SymbolInformation,
};
use crate::registry;

pub struct ScopeFind {
    pub scope: Option<String>,
    pub find: Option<String>,
}

fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| anyhow!("Cannot read file: {} ({e})", path.display()))
}

/// Prints the `--dry-run` preview shared by every navigation command: the
/// LSP request that would be sent, without sending it. Was previously
/// hand-rolled ~identically in 7 places (one per command), drifting slightly
/// each time a command was added — `calls`'s version, for instance, built
/// its `method` field differently from the rest before this was extracted.
fn print_dry_run(
    project_root: impl serde::Serialize,
    language: Option<&str>,
    method: &str,
    params: Value,
) {
    let mut obj = json!({ "dry_run": true, "project_root": project_root, "method": method, "params": params });
    if let Some(lang) = language {
        obj["language"] = json!(lang);
    }
    println!("{obj}");
}

/// Ensures the daemon is running, that it has a warm (possibly newly
/// spawned, possibly reused) server for `ctx`'s project, and that the
/// target file is open in it — then returns a client ready for
/// `proxy_request` calls against `ctx.project_root`.
///
/// Auto-installs a missing language server *before* contacting the daemon
/// (rather than leaving that to `Manager::create` on the daemon side) so
/// install progress prints to the user's own terminal — the daemon's stdio
/// is normally discarded when auto-spawned by `ensure_running`, so an
/// install happening there would look like the CLI silently hanging.
///
/// Skipped entirely when the daemon already reports a running warm server
/// for this exact project+language: `ensure_installed` otherwise spawns a
/// `<bin> --version` subprocess (a real node/JVM startup cost for several
/// languages) on *every single navigation command*, even though a live
/// server is direct proof the binary is present and working. A running
/// server having its binary deleted out from under it mid-session is not a
/// case worth paying that cost on every call to guard against.
async fn ensure_daemon_session(ctx: &ProjectContext, content: &str) -> Result<ManagerClient> {
    let client = ManagerClient::new();
    let project_root = ctx.project_root.to_string_lossy();
    let already_warm = client.is_alive().await
        && client
            .list_servers()
            .await
            .map(|servers| {
                servers.iter().any(|s| {
                    s.project_root == project_root
                        && s.language == ctx.language
                        && s.status == "running"
                })
            })
            .unwrap_or(false);

    if !already_warm {
        crate::install::ensure_installed(&ctx.language).await?;
    }

    client.ensure_running().await?;
    client
        .create_server(&ctx.file_path.to_string_lossy())
        .await?;
    // The daemon (`Manager::proxy_notify`) turns this into a `didChange`
    // instead of a second `didOpen` when the file is already open in this
    // warm server — required, not just an optimization: typescript-language-
    // server rejects a duplicate `didOpen` on an already-open document and
    // silently skips reprocessing it, which starves diagnostics/analysis of
    // ever re-running against the current content on a warm-reuse call.
    client
        .proxy_notify(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": ctx.uri,
                    "languageId": language_id(&ctx.language),
                    "version": 1,
                    "text": content,
                }
            }),
        )
        .await?;
    // Give the server a moment to build its AST after didOpen. Warm reuse
    // only saves the (usually dominant) process-spawn + `initialize` cost.
    // Delay chosen empirically: 800ms produced wrong/unresolved `definition`
    // results under system load; 1500ms was reliable for a single warm
    // server but still failed once multiple *different* language servers
    // were warm and running concurrently — a new failure mode this reuse
    // feature itself introduces (several servers competing for CPU during
    // each other's analysis passes). 3000ms was reliable in that
    // adversarial case (verified: `tests/web.rs` running css, html, and
    // json commands back-to-back against three simultaneously-warm
    // servers). See README "Reliability fixes" for the measurements.
    tokio::time::sleep(std::time::Duration::from_millis(DIDOPEN_SETTLE_DELAY_MS)).await;
    Ok(client)
}

/// How many extra attempts `proxy_request_with_retry` makes beyond the
/// first, when the result still looks "not ready" (empty/null). Backoff is
/// `RETRY_BACKOFF_MS * attempt_number`.
const MAX_EMPTY_RESULT_RETRIES: u32 = 3;
const RETRY_BACKOFF_MS: u64 = 500;

/// `ensure_daemon_session`'s fixed post-`didOpen` delay is an empirically
/// tuned average, not a guarantee — under heavier system load (e.g. several
/// warm servers competing for CPU, as happens when the test suite runs many
/// language-server-backed integration tests concurrently), a server can
/// still be mid-indexing once that delay elapses, and `definition`/`hover`
/// come back empty/null even though the symbol genuinely exists. Reproduced
/// live: `rust_lang.rs`'s cross-file `definition` and hover tests flaked
/// under concurrent-suite load despite passing reliably in isolation.
///
/// LSP requests like `definition`/`hover` are read-only and idempotent, so
/// retrying the exact same request after a short backoff is safe. This is
/// deliberately generic (any command can opt in via `is_empty`) rather than
/// hardcoded to rust-analyzer, since the same indexing-lag class of
/// flakiness applies to any server that does background indexing (gopls,
/// clangd, etc.) — the caller decides what "not ready yet" looks like for
/// its own result shape.
async fn proxy_request_with_retry(
    client: &ManagerClient,
    project_root: &str,
    language: &str,
    method: &str,
    params: Value,
    is_empty: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let mut result = client
        .proxy_request(project_root, Some(language), method, params.clone())
        .await?;
    let mut attempt = 1;
    while is_empty(&result) && attempt <= MAX_EMPTY_RESULT_RETRIES {
        tokio::time::sleep(std::time::Duration::from_millis(
            RETRY_BACKOFF_MS * attempt as u64,
        ))
        .await;
        result = client
            .proxy_request(project_root, Some(language), method, params.clone())
            .await?;
        attempt += 1;
    }
    Ok(result)
}

fn is_empty_locations_result(v: &Value) -> bool {
    v.is_null() || v.as_array().is_some_and(|a| a.is_empty())
}

// ---------------------------------------------------------------------------
// outline
// ---------------------------------------------------------------------------

pub async fn run_outline(
    file: &str,
    all: bool,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;

    if dry_run {
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": ctx.uri}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = client
        .proxy_request(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": ctx.uri } }),
        )
        .await?;

    let symbols: Vec<DocumentSymbol> = serde_json::from_value(result).unwrap_or_default();
    let filtered = if all {
        symbols
    } else {
        filter_top_level(symbols)
    };
    println!("{}", fmt.outline(&filtered));
    Ok(())
}

// ---------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------

pub async fn run_diagnostics(
    file: &str,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;

    if dry_run {
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            "textDocument/diagnostic",
            json!({"textDocument": {"uri": ctx.uri}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = client
        .proxy_request(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": ctx.uri } }),
        )
        .await
        .map_err(|e| {
            anyhow!(
                "{e}\n\nHint: not every language server supports pull diagnostics \
                 (LSP 3.17 textDocument/diagnostic) yet. If this keeps failing for \
                 {}, that server doesn't support this command.",
                ctx.language
            )
        })?;

    let report: DocumentDiagnosticReport = serde_json::from_value(result).unwrap_or_default();
    println!("{}", fmt.diagnostics(&report.items));
    Ok(())
}

// ---------------------------------------------------------------------------
// calls (call hierarchy)
// ---------------------------------------------------------------------------

pub async fn run_calls(
    file: &str,
    sf: ScopeFind,
    direction: &str,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    if direction != "incoming" && direction != "outgoing" {
        bail!("Unknown direction: {direction} (expected one of: incoming, outgoing)");
    }

    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;

    if dry_run {
        let calls_method = if direction == "incoming" {
            "callHierarchy/incomingCalls"
        } else {
            "callHierarchy/outgoingCalls"
        };
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            &format!("textDocument/prepareCallHierarchy -> {calls_method}"),
            json!({"textDocument": {"uri": ctx.uri}, "position": {"line": pos.line, "character": pos.character}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let project_root = ctx.project_root.to_string_lossy();

    let prepared = client
        .proxy_request(
            &project_root,
            Some(&ctx.language),
            "textDocument/prepareCallHierarchy",
            json!({ "textDocument": { "uri": ctx.uri }, "position": { "line": pos.line, "character": pos.character } }),
        )
        .await?;
    let items: Vec<CallHierarchyItem> = serde_json::from_value(prepared).unwrap_or_default();
    let Some(root) = items.into_iter().next() else {
        println!("{}", fmt.calls(direction, &[]));
        return Ok(());
    };

    let root_json = serde_json::to_value(&root)?;

    let items = if direction == "incoming" {
        let result = client
            .proxy_request(
                &project_root,
                Some(&ctx.language),
                "callHierarchy/incomingCalls",
                json!({ "item": root_json }),
            )
            .await?;
        let calls: Vec<CallHierarchyIncomingCall> =
            serde_json::from_value(result).unwrap_or_default();
        calls.into_iter().map(|c| c.from).collect::<Vec<_>>()
    } else {
        let result = client
            .proxy_request(
                &project_root,
                Some(&ctx.language),
                "callHierarchy/outgoingCalls",
                json!({ "item": root_json }),
            )
            .await?;
        let calls: Vec<CallHierarchyOutgoingCall> =
            serde_json::from_value(result).unwrap_or_default();
        calls.into_iter().map(|c| c.to).collect::<Vec<_>>()
    };

    println!("{}", fmt.calls(direction, &items));
    Ok(())
}

fn filter_top_level(symbols: Vec<DocumentSymbol>) -> Vec<DocumentSymbol> {
    use crate::protocol::symbol_kind::{
        CLASS, CONSTRUCTOR, ENUM, FUNCTION, INTERFACE, METHOD, MODULE, NAMESPACE, PROPERTY, STRUCT,
    };
    const TOP: &[u32] = &[CLASS, INTERFACE, ENUM, FUNCTION, MODULE, NAMESPACE, STRUCT];
    symbols
        .into_iter()
        .filter(|s| TOP.contains(&s.kind))
        .map(|mut s| {
            s.children = s.children.map(|c| {
                c.into_iter()
                    .filter(|c| matches!(c.kind, METHOD | CONSTRUCTOR | PROPERTY))
                    .collect()
            });
            s
        })
        .collect()
}

// ---------------------------------------------------------------------------
// definition
// ---------------------------------------------------------------------------

pub async fn run_definition(
    file: &str,
    sf: ScopeFind,
    mode: &str,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;
    let method = match mode {
        "definition" => "textDocument/definition",
        "declaration" => "textDocument/declaration",
        "type_definition" => "textDocument/typeDefinition",
        other => bail!(
            "Unknown mode: {other} (expected one of: definition, declaration, type_definition)"
        ),
    };

    if dry_run {
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            method,
            json!({"textDocument": {"uri": ctx.uri}, "position": {"line": pos.line, "character": pos.character}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = proxy_request_with_retry(
        &client,
        &ctx.project_root.to_string_lossy(),
        &ctx.language,
        method,
        json!({ "textDocument": { "uri": ctx.uri }, "position": { "line": pos.line, "character": pos.character } }),
        is_empty_locations_result,
    )
    .await?;

    let locations: Vec<Location> = if result.is_null() {
        vec![]
    } else {
        serde_json::from_value::<LocationOrMany>(result)?.into_vec()
    };
    println!("{}", fmt.definition(&locations));
    Ok(())
}

// ---------------------------------------------------------------------------
// reference
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn run_reference(
    file: &str,
    sf: ScopeFind,
    mode: &str,
    project: Option<&str>,
    dry_run: bool,
    max_items: usize,
    start_index: usize,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;
    let method = match mode {
        "references" => "textDocument/references",
        "implementations" => "textDocument/implementation",
        other => bail!("Unknown mode: {other} (expected one of: references, implementations)"),
    };

    if dry_run {
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            method,
            json!({"textDocument": {"uri": ctx.uri}, "position": {"line": pos.line, "character": pos.character}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = client
        .proxy_request(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            method,
            json!({
                "textDocument": { "uri": ctx.uri },
                "position": { "line": pos.line, "character": pos.character },
                "context": { "includeDeclaration": false }
            }),
        )
        .await?;

    let all_locations: Vec<Location> = serde_json::from_value(result).unwrap_or_default();
    let end = (start_index + max_items).min(all_locations.len());
    let page = if start_index < all_locations.len() {
        &all_locations[start_index..end]
    } else {
        &[]
    };
    println!("{}", fmt.reference(page));

    let remaining = all_locations.len().saturating_sub(start_index + page.len());
    if remaining > 0 {
        eprintln!(
            "\n[{remaining} more results — use --start-index {} to continue]",
            start_index + max_items
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// doc
// ---------------------------------------------------------------------------

pub async fn run_doc(
    file: &str,
    sf: ScopeFind,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;

    if dry_run {
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            "textDocument/hover",
            json!({"textDocument": {"uri": ctx.uri}, "position": {"line": pos.line, "character": pos.character}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = proxy_request_with_retry(
        &client,
        &ctx.project_root.to_string_lossy(),
        &ctx.language,
        "textDocument/hover",
        json!({ "textDocument": { "uri": ctx.uri }, "position": { "line": pos.line, "character": pos.character } }),
        Value::is_null,
    )
    .await?;

    if result.is_null() {
        println!(
            "{}",
            fmt.error("No documentation available for this symbol.")
        );
        return Ok(());
    }
    let hover: HoverResult = serde_json::from_value(result)?;
    println!("{}", fmt.hover(&hover));
    Ok(())
}

// ---------------------------------------------------------------------------
// symbol
// ---------------------------------------------------------------------------

pub async fn run_symbol(
    file: &str,
    sf: ScopeFind,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;

    if dry_run {
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": ctx.uri}}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = client
        .proxy_request(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": ctx.uri } }),
        )
        .await?;

    let symbols: Vec<DocumentSymbol> = serde_json::from_value(result).unwrap_or_default();
    let lines: Vec<&str> = content.split('\n').collect();

    let target = find_deepest_containing(&symbols, pos.line);
    let Some(target) = target else {
        eprintln!(
            "{}",
            fmt.error(&format!("No symbol found at line {}", pos.line + 1))
        );
        std::process::exit(1);
    };

    let end = (target.range.end.line as usize + 1).min(lines.len());
    let start = (target.range.start.line as usize).min(end);
    let source = lines[start..end].join("\n");
    println!("{}", fmt.symbol_source(&target.name, target.kind, &source));
    Ok(())
}

fn find_deepest_containing(symbols: &[DocumentSymbol], line: u32) -> Option<DocumentSymbol> {
    let mut deepest = None;
    fn visit(syms: &[DocumentSymbol], line: u32, deepest: &mut Option<DocumentSymbol>) {
        for sym in syms {
            if sym.range.start.line <= line && line <= sym.range.end.line {
                *deepest = Some(sym.clone());
                if let Some(children) = &sym.children {
                    visit(children, line, deepest);
                }
            }
        }
    }
    visit(symbols, line, &mut deepest);
    deepest
}

// ---------------------------------------------------------------------------
// locate
// ---------------------------------------------------------------------------

pub fn run_locate(file: &str, sf: ScopeFind, fmt: &OutputFormat) -> Result<()> {
    let abs = Path::new(file)
        .canonicalize()
        .map_err(|_| anyhow!("File not found: {file}"))?;
    let content = read_file(&abs)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;
    let lines: Vec<&str> = content.split('\n').collect();

    let ctx_start = pos.line.saturating_sub(3) as usize;
    let ctx_end = ((pos.line + 3) as usize).min(lines.len().saturating_sub(1));
    let context_lines = &lines[ctx_start..=ctx_end.min(lines.len().saturating_sub(1))];

    match fmt {
        OutputFormat::Markdown => {
            let line_num = pos.line + 1;
            let char_num = pos.character + 1;
            println!("Resolved: {}:{}:{}\n", abs.display(), line_num, char_num);
            for (i, line) in context_lines.iter().enumerate() {
                let n = ctx_start + i + 1;
                let marker = if n as u32 == line_num {
                    "\u{2192}"
                } else {
                    " "
                };
                println!("{marker} {:>4} \u{2502} {line}", n);
            }
        }
        OutputFormat::Json => {
            let context: Vec<_> = context_lines
                .iter()
                .enumerate()
                .map(|(i, text)| json!({ "line": ctx_start + i + 1, "text": text, "isCursor": (ctx_start + i) as u32 == pos.line }))
                .collect();
            println!(
                "{}",
                json!({ "kind": "locate", "file": abs, "line": pos.line + 1, "character": pos.character, "context": context })
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// search (LSP workspace/symbol, falling back to BM25)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn run_search(
    query: &str,
    kinds: Option<Vec<String>>,
    project: Option<&str>,
    dry_run: bool,
    max_items: usize,
    start_index: usize,
    fmt: &OutputFormat,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project_root = match project {
        Some(p) => p.to_string(),
        None => {
            // Best-effort auto-detect, same probing strategy as search.ts.
            registry::detect_project_root(&cwd.join("index.ts"))
                .or_else(|| registry::detect_project_root(&cwd.join("main.go")))
                .or_else(|| registry::detect_project_root(&cwd.join("main.py")))
                .map(|d| d.root.to_string_lossy().to_string())
                .unwrap_or_else(|| cwd.to_string_lossy().to_string())
        }
    };

    if dry_run {
        print_dry_run(
            &project_root,
            None,
            "workspace/symbol",
            json!({"query": query}),
        );
        return Ok(());
    }

    // Try LSP (via the warm daemon-managed server, same as the other
    // navigation commands) if a project language can be detected; otherwise
    // (or on any failure — including "no server installed", which this path
    // does not attempt to auto-install, matching the TS original's search.ts)
    // fall back to the self-built BM25 index.
    let mut results: Vec<SymbolInformation> = try_lsp_search(&project_root, query)
        .await
        .unwrap_or_default();

    if results.is_empty() {
        let index = Bm25Index::build(&project_root);
        results = index
            .search(query)
            .into_iter()
            .map(|(_, s)| s.clone())
            .collect();
    }

    if let Some(kinds) = kinds {
        let kind_ids: std::collections::HashSet<u32> = (1u32..=26)
            .filter(|k| kinds.iter().any(|name| symbol_kind_name(*k) == name))
            .collect();
        results.retain(|s| kind_ids.contains(&s.kind));
    }

    let total = results.len();
    let end = (start_index + max_items).min(total);
    let page = if start_index < total {
        &results[start_index..end]
    } else {
        &[]
    };

    match fmt {
        OutputFormat::Markdown => {
            if page.is_empty() {
                println!("No matches found.");
            } else {
                for (i, sym) in page.iter().enumerate() {
                    let file_path = sym
                        .location
                        .uri
                        .strip_prefix("file://")
                        .unwrap_or(&sym.location.uri);
                    println!(
                        "{}. [{}] {}  {}:{}",
                        i + start_index + 1,
                        symbol_kind_name(sym.kind),
                        sym.name,
                        file_path,
                        sym.location.range.start.line + 1
                    );
                }
            }
            let remaining = total.saturating_sub(start_index + page.len());
            if remaining > 0 {
                println!(
                    "\n[{remaining} more — use --start-index {} ]",
                    start_index + max_items
                );
            }
        }
        OutputFormat::Json => {
            let items: Vec<_> = page
                .iter()
                .map(|sym| {
                    json!({
                        "name": sym.name,
                        "kind": symbol_kind_name(sym.kind),
                        "uri": sym.location.uri.strip_prefix("file://").unwrap_or(&sym.location.uri),
                        "line": sym.location.range.start.line + 1,
                        "containerName": sym.container_name,
                    })
                })
                .collect();
            println!(
                "{}",
                json!({ "kind": "search", "query": query, "items": items, "total": total, "startIndex": start_index })
            );
        }
    }

    Ok(())
}

async fn try_lsp_search(project_root: &str, query: &str) -> Result<Vec<SymbolInformation>> {
    let root_path = Path::new(project_root);
    // Find any recognized source file directly under the project root to determine
    // which language server to launch.
    let (entry, language) = walkdir::WalkDir::new(root_path)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .find_map(|e| registry::detect_language(e.path()).map(|lang| (e, lang.name)))
        .ok_or_else(|| anyhow!("no recognizable source file"))?;

    let client = ManagerClient::new();
    client.ensure_running().await?;
    client
        .create_server(&entry.path().to_string_lossy())
        .await?;
    let result = client
        .proxy_request(
            project_root,
            Some(language),
            "workspace/symbol",
            json!({ "query": query }),
        )
        .await?;
    Ok(serde_json::from_value(result).unwrap_or_default())
}

// install/run_install_list moved to install.rs, which does real installation
// (npm/go install/GitHub releases) instead of just reporting paths.

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

pub fn run_schema(command: Option<&str>) -> Result<()> {
    let schemas = crate::schema::schemas();
    match command {
        None => println!("{}", serde_json::to_string_pretty(&schemas)?),
        Some(name) => match schemas.get(name) {
            Some(s) => println!("{}", serde_json::to_string_pretty(s)?),
            None => bail!(
                "Unknown command '{name}'. Available: {}",
                schemas.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        },
    }
    Ok(())
}
