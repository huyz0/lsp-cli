//! Command implementations. Each `run_*` function mirrors the corresponding
//! commands/*.ts file. Navigation commands (outline/definition/reference/
//! doc/symbol/search) proxy their LSP traffic through the background daemon
//! (`src/daemon.rs`) via `ensure_daemon_session`, so a language server
//! started for a project is reused warm across CLI invocations — including
//! across separate OS processes — instead of being spawned and killed fresh
//! on every single command. See docs/architecture.md ("Manager daemon" /
//! "Warm server reuse") for how that fits together.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use lsp::text_pos::utf16_col_to_byte;

/// How long to wait after `didOpen`/`didChange` before issuing the actual
/// request, giving the server time to build its AST.
///
/// Still a sleep rather than a poll, because a *warm* server has no
/// observable "done" signal for a single-document change — but it is now
/// sized to the situation instead of being one worst-case constant paid by
/// everything.
///
/// - **Bundled servers** parse synchronously inside the request handler
///   (`src/servers/`, tree-sitter), so there is nothing to wait for at all.
///   They were paying three seconds per command for no reason.
/// - **A warm server** only has to digest the one document that changed,
///   which is what the original 3000ms was actually measured against
///   (several warm servers competing for CPU).
///
/// The cold-start case is handled where it belongs, in the daemon: see
/// `daemon.rs::wait_until_indexed`, which polls for readiness under the
/// per-project create lock so every caller benefits, not just whichever
/// one happened to spawn the server.
const BUNDLED_SETTLE_DELAY_MS: u64 = 0;
const WARM_SETTLE_DELAY_MS: u64 = 3000;

fn settle_delay(language: &str) -> std::time::Duration {
    let ms = if registry::is_bundled_language(language) {
        BUNDLED_SETTLE_DELAY_MS
    } else {
        WARM_SETTLE_DELAY_MS
    };
    std::time::Duration::from_millis(ms)
}

use crate::bm25::{is_ignored_dir_name, Bm25Index};
use crate::format::OutputFormat;
use crate::locate::resolve_locate;
use crate::manager_client::ManagerClient;
use crate::project::{language_id, resolve_project, ProjectContext};
use crate::protocol::{
    symbol_kind_name, CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall,
    DocumentChangeOp, DocumentDiagnosticReport, DocumentSymbol, HoverResult, Location,
    LocationOrMany, SymbolInformation, TextEdit, TypeHierarchyItem, WorkspaceEdit,
    ALL_SYMBOL_KIND_IDS,
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
        .create_server(
            &ctx.file_path.to_string_lossy(),
            Some(&ctx.project_root.to_string_lossy()),
        )
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
    // The warm figure was chosen empirically: 800ms produced wrong/
    // unresolved `definition` results under system load; 1500ms was
    // reliable for a single warm server but still failed once multiple
    // *different* language servers were warm and running concurrently
    // (several servers competing for CPU during each other's analysis
    // passes). 3000ms was reliable in that adversarial case — verified by
    // `tests/web.rs` running css, html and json commands back-to-back
    // against three simultaneously-warm servers. See `settle_delay` for
    // why cold and bundled servers get different numbers.
    let delay = settle_delay(&ctx.language);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
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

// ---------------------------------------------------------------------------
// hierarchy
// ---------------------------------------------------------------------------

pub async fn run_hierarchy(
    file: &str,
    sf: ScopeFind,
    direction: &str,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    if direction != "supertypes" && direction != "subtypes" {
        bail!("Unknown direction: {direction} (expected one of: subtypes, supertypes)");
    }

    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;

    if dry_run {
        let method = if direction == "supertypes" {
            "typeHierarchy/supertypes"
        } else {
            "typeHierarchy/subtypes"
        };
        print_dry_run(
            &ctx.project_root,
            Some(&ctx.language),
            &format!("textDocument/prepareTypeHierarchy -> {method}"),
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
            "textDocument/prepareTypeHierarchy",
            json!({ "textDocument": { "uri": ctx.uri }, "position": { "line": pos.line, "character": pos.character } }),
        )
        .await?;
    let items: Vec<TypeHierarchyItem> = serde_json::from_value(prepared).unwrap_or_default();
    let Some(root) = items.into_iter().next() else {
        println!("{}", fmt.hierarchy(direction, &[]));
        return Ok(());
    };
    let root_json = serde_json::to_value(&root)?;

    let method = if direction == "supertypes" {
        "typeHierarchy/supertypes"
    } else {
        "typeHierarchy/subtypes"
    };
    let result = client
        .proxy_request(
            &project_root,
            Some(&ctx.language),
            method,
            json!({ "item": root_json }),
        )
        .await?;
    let items: Vec<TypeHierarchyItem> = serde_json::from_value(result).unwrap_or_default();

    println!("{}", fmt.hierarchy(direction, &items));
    Ok(())
}

// ---------------------------------------------------------------------------
// rename
// ---------------------------------------------------------------------------

/// Groups a `WorkspaceEdit`'s per-file text edits regardless of which of
/// the two shapes the server used — `documentChanges` (preferred when
/// present per spec, since it can carry document versions) or the older
/// flat `changes` map. `documentChanges` entries that are file operations
/// (create/rename/delete a file, not a text edit) aren't applied by this
/// tool; their count is returned separately so the caller can surface
/// "N operations skipped" instead of silently treating the rename as fully
/// applied when it wasn't.
fn collect_edits(edit: &WorkspaceEdit) -> (Vec<(String, Vec<TextEdit>)>, usize) {
    if let Some(doc_changes) = &edit.document_changes {
        let mut files = Vec::new();
        let mut skipped = 0;
        for op in doc_changes {
            match op {
                DocumentChangeOp::Edit(te) => {
                    files.push((te.text_document.uri.clone(), te.edits.clone()))
                }
                DocumentChangeOp::FileOp(_) => skipped += 1,
            }
        }
        return (files, skipped);
    }
    if let Some(changes) = &edit.changes {
        return (
            changes
                .iter()
                .map(|(uri, edits)| (uri.clone(), edits.clone()))
                .collect(),
            0,
        );
    }
    (vec![], 0)
}

/// Applies `edits` to `content` and returns the new text. Edits are applied
/// in reverse position order (bottom-to-top, right-to-left within a line)
/// so that applying one edit never invalidates the line/character offsets
/// of edits still pending — the offsets in a `WorkspaceEdit` are all
/// relative to the *original* unmodified document, per the LSP spec.
///
/// Character offsets are UTF-16 code units, per the spec and per what
/// `lsp_client.rs::initialize` negotiates (it declares no
/// `positionEncodings`, so UTF-16 is mandatory). This used to index a
/// `Vec<char>` with those offsets, which is only correct while every
/// character on the line is in the Basic Multilingual Plane: a single
/// astral character (emoji, `𝕏`) earlier on the line shifts every
/// subsequent offset by one and the edit lands in the wrong place. Since
/// this is the one code path in the tool that writes to disk, that
/// mis-slice silently corrupted the file rather than merely returning a
/// wrong answer.
fn apply_text_edits(content: &str, edits: &[TextEdit]) -> String {
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then(b.range.start.character.cmp(&a.range.start.character))
    });

    for edit in sorted {
        let start_line = edit.range.start.line as usize;
        let end_line = edit.range.end.line as usize;
        if start_line >= lines.len() {
            continue; // stale edit against content that's shifted since the server computed it
        }
        if start_line == end_line {
            let line = &lines[start_line];
            let start_b = utf16_col_to_byte(line, edit.range.start.character);
            let end_b = utf16_col_to_byte(line, edit.range.end.character).max(start_b);
            lines[start_line] = format!("{}{}{}", &line[..start_b], edit.new_text, &line[end_b..]);
        } else if end_line < lines.len() {
            let start_b = utf16_col_to_byte(&lines[start_line], edit.range.start.character);
            let end_b = utf16_col_to_byte(&lines[end_line], edit.range.end.character);
            let merged = format!(
                "{}{}{}",
                &lines[start_line][..start_b],
                edit.new_text,
                &lines[end_line][end_b..]
            );
            lines.splice(start_line..=end_line, [merged]);
        }
    }
    lines.join("\n")
}

pub async fn run_rename(
    file: &str,
    sf: ScopeFind,
    new_name: &str,
    apply: bool,
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
            "textDocument/rename",
            json!({"textDocument": {"uri": ctx.uri}, "position": {"line": pos.line, "character": pos.character}, "newName": new_name}),
        );
        return Ok(());
    }

    let client = ensure_daemon_session(&ctx, &content).await?;
    let result = client
        .proxy_request(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            "textDocument/rename",
            json!({ "textDocument": { "uri": ctx.uri }, "position": { "line": pos.line, "character": pos.character }, "newName": new_name }),
        )
        .await?;

    if result.is_null() {
        println!(
            "{}",
            fmt.error("No rename edits returned — the server may not support renaming this symbol, or the position doesn't resolve to a renameable symbol. Run `lsp locate` first to confirm the position resolves where you expect.")
        );
        return Ok(());
    }
    let edit: WorkspaceEdit = serde_json::from_value(result)?;
    let (files_with_edits, skipped_ops) = collect_edits(&edit);

    if apply {
        // Two phases on purpose. Reading every file and computing every
        // replacement *before* writing any of them means an unreadable
        // file (or one whose edits don't apply) aborts with the workspace
        // untouched. Interleaving read/write per file, as this used to,
        // left files 1..N-1 renamed and the rest not — a silently
        // half-renamed codebase, which is the specific failure the
        // preview-by-default design exists to avoid.
        let mut staged: Vec<(PathBuf, String)> = Vec::with_capacity(files_with_edits.len());
        for (uri, edits) in &files_with_edits {
            let path = lsp::uri::to_path(uri);
            let original = std::fs::read_to_string(&path).map_err(|e| {
                anyhow!(
                    "Cannot read {} to apply rename (no files were modified): {e}",
                    path.display()
                )
            })?;
            staged.push((path, apply_text_edits(&original, edits)));
        }
        for (path, updated) in staged {
            std::fs::write(&path, updated)
                .map_err(|e| anyhow!("Cannot write {}: {e}", path.display()))?;
        }
    }

    println!(
        "{}",
        fmt.rename(new_name, apply, &files_with_edits, skipped_ops)
    );
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
        // Same shape as `doc` and `rename` report their own "nothing
        // here" case: a formatted result on stdout, exit 0. This used to
        // write to stderr and `std::process::exit(1)` from inside a
        // library function — skipping destructors, unreachable from a
        // test without spawning a subprocess, and leaving an agent that
        // captures stdout with nothing at all to parse.
        println!(
            "{}",
            fmt.error(&format!("No symbol found at line {}", pos.line + 1))
        );
        return Ok(());
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
        // Reject unknown values rather than filtering everything away. An
        // unrecognized `--kinds` used to contribute nothing to the id set,
        // so `--kinds klass` returned zero results and exit 0 — a silent
        // wrong answer, indistinguishable from "no such symbol", and the
        // opposite of how every other enum-valued flag here behaves.
        let mut unknown: Vec<&str> = kinds
            .iter()
            .filter(|name| {
                !ALL_SYMBOL_KIND_IDS
                    .iter()
                    .any(|k| symbol_kind_name(*k) == *name)
            })
            .map(|s| s.as_str())
            .collect();
        unknown.sort_unstable();
        if !unknown.is_empty() {
            let valid: Vec<&str> = ALL_SYMBOL_KIND_IDS
                .iter()
                .map(|k| symbol_kind_name(*k))
                .collect();
            bail!(
                "Unknown --kinds value(s): {} (expected one of: {})",
                unknown.join(", "),
                valid.join(", ")
            );
        }
        let kind_ids: std::collections::HashSet<u32> = ALL_SYMBOL_KIND_IDS
            .iter()
            .copied()
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
    // Skip the same directories the BM25 indexer skips. Without this the
    // "representative source file" could be picked out of `node_modules/`,
    // `target/`, or `.git/`, which both wastes the walk and can start a
    // server rooted at a vendored copy of someone else's code. The
    // `depth() == 0` guard keeps the root itself from being pruned when
    // the project directory is a dotfile directory (`~/.dotfiles`).
    let (entry, _) = walkdir::WalkDir::new(root_path)
        .max_depth(4)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_ignored_dir_name(&e.file_name().to_string_lossy()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .find_map(|e| registry::detect_language(e.path()).map(|lang| (e, lang.name)))
        .ok_or_else(|| anyhow!("no recognizable source file"))?;

    let client = ManagerClient::new();
    client.ensure_running().await?;
    // Use the language the daemon actually registered, not the one
    // `detect_language` guessed. The two disagree for Deno: extension
    // detection deliberately skips `deno` (it shares `.ts` with
    // typescript), while the daemon's root detection prefers it when a
    // `deno.json` is present — so asking for "typescript" here never
    // matched the running server and every Deno search silently fell
    // through to the BM25 index.
    let info = client
        .create_server(&entry.path().to_string_lossy(), Some(project_root))
        .await?;
    let result = client
        .proxy_request(
            project_root,
            Some(&info.language),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Position, Range, TextDocumentEdit, VersionedTextDocumentIdentifier};

    fn edit(sl: u32, sc: u32, el: u32, ec: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line: sl,
                    character: sc,
                },
                end: Position {
                    line: el,
                    character: ec,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn apply_text_edits_single_line_replace() {
        let content = "fn greet() {}\n";
        let out = apply_text_edits(content, &[edit(0, 3, 0, 8, "say_hi")]);
        assert_eq!(out, "fn say_hi() {}\n");
    }

    #[test]
    fn apply_text_edits_uses_utf16_offsets_not_char_offsets() {
        // An astral character before the edit is where UTF-16 offsets and
        // `char` offsets diverge: "😀" is one char but two UTF-16 code
        // units, so the server reports `oldName` at columns 14..21 while
        // it sits at chars 13..20. Indexing a Vec<char> with the server's
        // numbers used to splice one character off, producing
        // `let s = "😀"; onewName);` — and, because this is the rename
        // write path, saving that to disk.
        let content = "let s = \"😀\"; oldName();\n";
        let out = apply_text_edits(content, &[edit(0, 14, 0, 21, "newName")]);
        assert_eq!(out, "let s = \"😀\"; newName();\n");
    }

    #[test]
    fn apply_text_edits_handles_bmp_characters() {
        // Accents and CJK are one UTF-16 unit each, so these offsets agree
        // under either interpretation — a guard that the fix didn't break
        // the majority case it used to get right.
        let content = "let café = 1; let oldName = 2;\n";
        let out = apply_text_edits(content, &[edit(0, 18, 0, 25, "newName")]);
        assert_eq!(out, "let café = 1; let newName = 2;\n");
    }

    #[test]
    fn apply_text_edits_multiline_splice_uses_utf16_offsets_on_both_ends() {
        let content = "let a = \"😀\"; start\nmiddle\nend \"😀\" tail\n";
        // Start col 14 is just past the emoji on line 0. On line 2,
        // `end "😀" tail`, the emoji occupies UTF-16 columns 5-6, so
        // column 8 is the space before `tail` and column 9 is its `t`.
        let out = apply_text_edits(content, &[edit(0, 14, 2, 8, "X")]);
        assert_eq!(out, "let a = \"😀\"; X tail\n");
    }

    #[test]
    fn apply_text_edits_multiple_edits_same_file_dont_shift_each_other() {
        // Two edits on different lines, applied together — since
        // apply_text_edits sorts and applies bottom-to-top, the first
        // edit's line/character offsets must not be invalidated by the
        // second edit changing line lengths above it.
        let content = "fn greet() {}\n\nfn call() {\n    greet();\n}\n";
        let edits = vec![edit(0, 3, 0, 8, "say_hi"), edit(3, 4, 3, 9, "say_hi")];
        let out = apply_text_edits(content, &edits);
        assert_eq!(out, "fn say_hi() {}\n\nfn call() {\n    say_hi();\n}\n");
    }

    #[test]
    fn apply_text_edits_multiline_range_splices_correctly() {
        // Range starts right after "(" on line 0 and ends right before ")"
        // on line 2, so both parens are already outside the edit range —
        // new_text only needs to replace the parameter list between them.
        let content = "fn greet(\n    name: &str\n) {}\n";
        let out = apply_text_edits(content, &[edit(0, 9, 2, 0, "")]);
        assert_eq!(out, "fn greet() {}\n");
    }

    #[test]
    fn apply_text_edits_out_of_range_line_is_skipped_not_panicking() {
        // Defends against a stale WorkspaceEdit computed against content
        // that's since shrunk — must not panic on an out-of-bounds index.
        let content = "fn greet() {}\n";
        let out = apply_text_edits(content, &[edit(50, 0, 50, 5, "x")]);
        assert_eq!(out, content);
    }

    #[test]
    fn collect_edits_prefers_document_changes_over_flat_changes() {
        let doc_edit = TextDocumentEdit {
            text_document: VersionedTextDocumentIdentifier {
                uri: "file:///a.rs".into(),
            },
            edits: vec![edit(0, 0, 0, 1, "x")],
        };
        let mut changes = std::collections::HashMap::new();
        changes.insert("file:///b.rs".to_string(), vec![edit(0, 0, 0, 1, "y")]);
        let we = WorkspaceEdit {
            changes: Some(changes),
            document_changes: Some(vec![DocumentChangeOp::Edit(doc_edit)]),
        };
        let (files, skipped) = collect_edits(&we);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "file:///a.rs");
        assert_eq!(skipped, 0);
    }

    #[test]
    fn collect_edits_counts_file_operations_as_skipped_not_dropped_silently() {
        let doc_edit = TextDocumentEdit {
            text_document: VersionedTextDocumentIdentifier {
                uri: "file:///a.rs".into(),
            },
            edits: vec![edit(0, 0, 0, 1, "x")],
        };
        let file_op = serde_json::json!({"kind": "rename", "oldUri": "file:///old.rs", "newUri": "file:///new.rs"});
        let we = WorkspaceEdit {
            changes: None,
            document_changes: Some(vec![
                DocumentChangeOp::Edit(doc_edit),
                DocumentChangeOp::FileOp(file_op),
            ]),
        };
        let (files, skipped) = collect_edits(&we);
        assert_eq!(files.len(), 1);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn collect_edits_falls_back_to_flat_changes_when_no_document_changes() {
        let mut changes = std::collections::HashMap::new();
        changes.insert("file:///only.rs".to_string(), vec![edit(0, 0, 0, 1, "x")]);
        let we = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
        };
        let (files, skipped) = collect_edits(&we);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "file:///only.rs");
        assert_eq!(skipped, 0);
    }
}
