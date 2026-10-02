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

/// Optional pause after a document change, before the request
/// (`settleMs` in the config, default 0).
///
/// This used to be a fixed 3000ms sleep before *every* command against a
/// non-bundled server — about 99% of a warm command's wall time — even when
/// nothing had changed. It isn't needed for correctness: LSP servers handle
/// notifications and requests in the order received, so a request sent
/// after a `didChange` is answered against the changed text. What a sleep
/// used to paper over is now handled where it matters:
///
/// - **unchanged documents** send nothing, so there is nothing to wait for;
/// - **push diagnostics** wait in the daemon for a publish newer than the
///   latest edit (`daemon::PUSH_DIAGNOSTICS_WAIT`);
/// - **a server still catching up** (just started, or just given a change)
///   answers empty or with an error, which `proxy_request_with_retry`
///   retries — only in that situation, so a genuinely empty answer from a
///   warm server comes back immediately.
///
/// The knob remains for a server that turns out to answer stale-but-
/// non-empty after a change.
fn settle_delay(language: &str) -> std::time::Duration {
    if registry::is_bundled_language(language) {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_millis(crate::config::load_config().settle_ms)
}

use crate::bm25::{is_ignored_dir_name, Bm25Index};
use crate::format::OutputFormat;
use crate::locate::resolve_locate;
use crate::manager_client::ManagerClient;
use crate::project::{language_id, resolve_project, ProjectContext};
use crate::protocol::{
    symbol_kind_name, CallHierarchyIncomingCall, CallHierarchyOutgoingCall, DocumentChangeOp,
    DocumentDiagnosticReport, DocumentSymbol, HoverResult, Location, LocationOrMany,
    SymbolInformation, TextEdit, TypeHierarchyItem, WorkspaceEdit, ALL_SYMBOL_KIND_IDS,
};
use crate::registry;
use crate::{CallDirection, DefinitionMode, HierarchyDirection, ReferenceMode};

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
    let mut client = ManagerClient::new();
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

    let server_bin = if already_warm {
        None
    } else {
        crate::install::ensure_installed(&ctx.language).await?
    };

    client.ensure_running().await?;
    let info = client
        .create_server(
            &ctx.file_path.to_string_lossy(),
            Some(&ctx.project_root.to_string_lossy()),
            server_bin.as_deref(),
            Some(&ctx.language),
        )
        .await?;
    // The daemon (`Manager::proxy_notify`) turns this into a `didChange`
    // instead of a second `didOpen` when the file is already open in this
    // warm server — required, not just an optimization: typescript-language-
    // server rejects a duplicate `didOpen` on an already-open document and
    // silently skips reprocessing it, which starves diagnostics/analysis of
    // ever re-running against the current content on a warm-reuse call.
    let changed = client
        .proxy_notify(
            &ctx.project_root.to_string_lossy(),
            Some(&ctx.language),
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": ctx.uri,
                    "languageId": language_id(&ctx.language, &ctx.file_path),
                    "version": 1,
                    "text": content,
                }
            }),
        )
        .await?;
    client.fresh = info.just_started || info.loading || changed;
    if changed {
        let delay = settle_delay(&ctx.language);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(client)
}

/// How many extra attempts `proxy_request_with_retry` makes beyond the
/// first, when the result still looks "not ready" (empty/null). Backoff is
/// `RETRY_BACKOFF_MS * attempt_number`.
const MAX_EMPTY_RESULT_RETRIES: u32 = 3;
const RETRY_BACKOFF_MS: u64 = 500;

/// Retries a read-only request while the server still looks like it is
/// warming up.
///
/// A server that has finished `initialize` has not necessarily finished
/// loading the project, and it signals that in two different ways:
///
/// - an **empty result**, which is what rust-analyzer does mid-index. This
///   is why the retry exists at all — `rust_lang.rs`'s cross-file
///   `definition` and hover tests flaked under concurrent-suite load while
///   passing reliably in isolation.
/// - an **error**, which is what gopls does: it answers hover and
///   definition with `no package metadata for file` until its initial load
///   completes. Reproduced live, and this used to propagate straight out
///   as a command failure because only empty results were retried.
///
/// The second case is also why the daemon's readiness poll
/// (`daemon.rs::wait_until_indexed`) is not sufficient on its own:
/// `documentSymbol` is answered from syntax alone, so it succeeds while
/// type information is still missing. Outline works, hover does not.
///
/// Retrying is safe because every request routed through here is read-only
/// and idempotent. A genuinely failing request (an unsupported method, say)
/// costs the full backoff before surfacing, which is the price of not
/// string-matching server-specific error text to guess what is transient.
async fn proxy_request_with_retry(
    client: &ManagerClient,
    project_root: &str,
    language: &str,
    method: &str,
    params: Value,
    is_empty: impl Fn(&Value) -> bool,
) -> Result<Value> {
    // A warm server that was given nothing new has nothing to catch up on:
    // an empty answer from it is the answer. Retrying anyway cost a genuine
    // "no definition here" 3s of backoff.
    let max_retries = if client.fresh {
        MAX_EMPTY_RESULT_RETRIES
    } else {
        0
    };
    let mut attempt = 0;
    loop {
        let outcome = client
            .proxy_request(project_root, Some(language), method, params.clone())
            .await;

        let still_warming = match &outcome {
            Ok(v) => is_empty(v),
            Err(_) => true,
        };
        if !still_warming || attempt >= max_retries {
            return outcome;
        }

        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_millis(
            RETRY_BACKOFF_MS * attempt as u64,
        ))
        .await;
    }
}

/// Decodes a list-shaped LSP result. `null` is a legitimate "nothing";
/// a reply of any other shape is an error.
///
/// Every decode here used to be `.unwrap_or_default()`, which turned a
/// response this tool failed to understand into an empty list and exit
/// code 0 — indistinguishable from "there are no references".
fn decode_list<T: serde::de::DeserializeOwned>(v: Value, what: &str) -> Result<Vec<T>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(v)
        .map_err(|e| anyhow!("unexpected {what} response from the server: {e}"))
}

/// `documentSymbol` may answer in either of two shapes: hierarchical
/// `DocumentSymbol[]`, or flat `SymbolInformation[]` (which some servers
/// send regardless of the client's stated preference). The flat form used
/// to fail to decode and silently produce an empty outline.
fn decode_document_symbols(v: Value) -> Result<Vec<DocumentSymbol>> {
    let is_flat = v
        .as_array()
        .and_then(|a| a.first())
        .is_some_and(|first| first.get("location").is_some());
    if !is_flat {
        return decode_list(v, "documentSymbol");
    }
    let flat: Vec<SymbolInformation> = decode_list(v, "documentSymbol")?;
    Ok(flat
        .into_iter()
        .map(|s| DocumentSymbol {
            name: s.name,
            detail: s.container_name,
            kind: s.kind,
            range: s.location.range,
            selection_range: s.location.range,
            children: None,
        })
        .collect())
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

    let symbols = decode_document_symbols(result)?;
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

    let report: DocumentDiagnosticReport = if result.is_null() {
        DocumentDiagnosticReport::default()
    } else {
        serde_json::from_value(result)
            .map_err(|e| anyhow!("unexpected diagnostics response from the server: {e}"))?
    };
    println!("{}", fmt.diagnostics(&report.items));
    Ok(())
}

// ---------------------------------------------------------------------------
// calls (call hierarchy)
// ---------------------------------------------------------------------------

pub async fn run_calls(
    file: &str,
    sf: ScopeFind,
    direction: CallDirection,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;

    if dry_run {
        let calls_method = if direction == CallDirection::Incoming {
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
    // The prepared item goes back to the server exactly as received. It
    // can carry a `data` field (and `tags`) that the server needs to
    // resolve the follow-up request; it used to be decoded into a struct
    // without them and re-encoded, which dropped both.
    let items: Vec<Value> = decode_list(prepared, "prepareCallHierarchy")?;
    let Some(root_json) = items.into_iter().next() else {
        println!("{}", fmt.calls(direction.as_str(), &[]));
        return Ok(());
    };

    let items = if direction == CallDirection::Incoming {
        let result = client
            .proxy_request(
                &project_root,
                Some(&ctx.language),
                "callHierarchy/incomingCalls",
                json!({ "item": root_json }),
            )
            .await?;
        let calls: Vec<CallHierarchyIncomingCall> = decode_list(result, "incomingCalls")?;
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
        let calls: Vec<CallHierarchyOutgoingCall> = decode_list(result, "outgoingCalls")?;
        calls.into_iter().map(|c| c.to).collect::<Vec<_>>()
    };

    println!("{}", fmt.calls(direction.as_str(), &items));
    Ok(())
}

// ---------------------------------------------------------------------------
// hierarchy
// ---------------------------------------------------------------------------

pub async fn run_hierarchy(
    file: &str,
    sf: ScopeFind,
    direction: HierarchyDirection,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;

    if dry_run {
        let method = if direction == HierarchyDirection::Supertypes {
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
    // Sent back verbatim; see the same step in `run_calls`.
    let items: Vec<Value> = decode_list(prepared, "prepareTypeHierarchy")?;
    let Some(root_json) = items.into_iter().next() else {
        println!("{}", fmt.hierarchy(direction.as_str(), &[]));
        return Ok(());
    };

    let method = if direction == HierarchyDirection::Supertypes {
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
    let items: Vec<TypeHierarchyItem> = decode_list(result, method)?;

    println!("{}", fmt.hierarchy(direction.as_str(), &items));
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
                // A file may appear more than once; its edits are one set,
                // all against the original text, and must be validated and
                // applied together.
                DocumentChangeOp::Edit(te) => {
                    match files
                        .iter_mut()
                        .find(|(uri, _): &&mut (String, Vec<TextEdit>)| {
                            *uri == te.text_document.uri
                        }) {
                        Some((_, edits)) => edits.extend(te.edits.iter().cloned()),
                        None => files.push((te.text_document.uri.clone(), te.edits.clone())),
                    }
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

/// Applies `edits` to `content` and returns the new text, or an error if
/// the edits can't be applied as a whole.
///
/// All offsets in a `WorkspaceEdit` refer to the *original* document (LSP
/// spec), so every edit is resolved to a byte span of `content` first and
/// the result is assembled in one pass. Character offsets are UTF-16 code
/// units (`initialize` negotiates no other `positionEncodings`): a
/// `Vec<char>` index used to put an edit in the wrong place after an astral
/// character, and this is the one code path that writes to disk.
///
/// Refused, rather than skipped, because a partially applied rename is a
/// silently broken codebase:
/// - a line past the end of the document (the edit was computed against
///   different content);
/// - a range whose end precedes its start;
/// - overlapping ranges.
///
/// A character past the end of its line is clamped to the line's end, which
/// is what the spec says it means (the line ending itself is never part of
/// the line). Several inserts at the same position are applied in the order
/// given, as the spec requires; the previous bottom-to-top sort reversed
/// them.
fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    // Byte offset of the start of each line. `split('\n')` yields the
    // empty "line" after a trailing newline, which is a valid position.
    let mut line_starts = vec![0usize];
    line_starts.extend(content.match_indices('\n').map(|(i, _)| i + 1));
    let line_text = |line: usize| -> &str {
        let start = line_starts[line];
        let end = line_starts
            .get(line + 1)
            .map(|next| next - 1)
            .unwrap_or(content.len());
        content[start..end]
            .strip_suffix('\r')
            .unwrap_or(&content[start..end])
    };
    let offset = |pos: &crate::protocol::Position| -> Result<usize> {
        let line = pos.line as usize;
        // The start of the line after the last one is the end of the
        // document, which is how servers spell "to the end" in a
        // whole-file edit of a file without a trailing newline.
        if line == line_starts.len() && pos.character == 0 {
            return Ok(content.len());
        }
        if line >= line_starts.len() {
            let real_lines = line_starts.len() - usize::from(content.ends_with('\n'));
            bail!(
                "edit refers to line {} but the file has {real_lines} line(s); it was computed against different content",
                line + 1
            );
        }
        Ok(line_starts[line] + utf16_col_to_byte(line_text(line), pos.character))
    };

    let mut spans: Vec<(usize, usize, usize, &str)> = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let start = offset(&edit.range.start)?;
        let end = offset(&edit.range.end)?;
        if end < start {
            bail!(
                "edit range ends before it starts (line {}:{} to {}:{})",
                edit.range.start.line + 1,
                edit.range.start.character,
                edit.range.end.line + 1,
                edit.range.end.character
            );
        }
        spans.push((start, end, index, &edit.new_text));
    }
    spans.sort_by_key(|&(start, end, index, _)| (start, end, index));
    for pair in spans.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!("the server returned overlapping edits; refusing to apply them");
        }
    }

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0;
    for (start, end, _, new_text) in spans {
        out.push_str(&content[cursor..start]);
        out.push_str(new_text);
        cursor = end;
    }
    out.push_str(&content[cursor..]);
    Ok(out)
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
        // An error, not a printed message with exit code 0: nothing was
        // renamed, and a caller checking the exit status must see that.
        bail!("No rename edits returned — the server may not support renaming this symbol, or the position doesn't resolve to a renameable symbol. Run `lsp locate` first to confirm the position resolves where you expect.");
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
            let updated = apply_text_edits(&original, edits).map_err(|e| {
                anyhow!(
                    "Cannot apply rename to {} (no files were modified): {e}",
                    path.display()
                )
            })?;
            staged.push((path, updated));
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
    mode: DefinitionMode,
    project: Option<&str>,
    dry_run: bool,
    fmt: &OutputFormat,
) -> Result<()> {
    let ctx = resolve_project(file, project)?;
    let content = read_file(&ctx.file_path)?;
    let pos = resolve_locate(&content, sf.scope.as_deref(), sf.find.as_deref())?;
    let method = match mode {
        DefinitionMode::Definition => "textDocument/definition",
        DefinitionMode::Declaration => "textDocument/declaration",
        DefinitionMode::TypeDefinition => "textDocument/typeDefinition",
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
    mode: ReferenceMode,
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
        ReferenceMode::References => "textDocument/references",
        ReferenceMode::Implementations => "textDocument/implementation",
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

    let all_locations: Vec<Location> = decode_list(result, "references")?;
    let end = start_index
        .saturating_add(max_items)
        .min(all_locations.len());
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
            start_index.saturating_add(max_items)
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

    let symbols = decode_document_symbols(result)?;
    let lines: Vec<&str> = content.split('\n').collect();

    let target = find_deepest_containing(&symbols, pos.line);
    let Some(target) = target else {
        // An error (exit 1, message on stderr) like every other "that
        // position doesn't identify what you asked for": the scope was
        // wrong, and a caller checking the exit status must see that. It
        // used to print an error-shaped document with exit 0. (`doc` with
        // no hover text is different: "no documentation" is an answer.)
        bail!(
            "No symbol found at line {}. Use `lsp outline` to see the file's symbols and their lines.",
            pos.line + 1
        );
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

    println!(
        "{}",
        fmt.locate(&abs, pos.line, pos.character, ctx_start, context_lines)
    );
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
        results = bm25_search(&project_root, query).await;
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
    let end = start_index.saturating_add(max_items).min(total);
    let page = if start_index < total {
        &results[start_index..end]
    } else {
        &[]
    };

    println!(
        "{}",
        fmt.search(
            query,
            page,
            total,
            start_index,
            start_index.saturating_add(max_items)
        )
    );
    Ok(())
}

/// BM25 fallback search.
///
/// Prefers the daemon, which caches the index per project and rebuilds it
/// only when the tree actually changes. Falls back to building in-process
/// if the daemon can't be reached, so `lsp search` still works with no
/// daemon available — just without the caching.
async fn bm25_search(project_root: &str, query: &str) -> Vec<SymbolInformation> {
    let client = ManagerClient::new();
    if client.ensure_running().await.is_ok() {
        if let Ok(results) = client.search(project_root, query).await {
            return results;
        }
    }
    Bm25Index::build(project_root)
        .search(query)
        .into_iter()
        .map(|(_, s)| s.clone())
        .collect()
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
        .create_server(
            &entry.path().to_string_lossy(),
            Some(project_root),
            None,
            None,
        )
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
        let out = apply_text_edits(content, &[edit(0, 3, 0, 8, "say_hi")]).unwrap();
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
        let out = apply_text_edits(content, &[edit(0, 14, 0, 21, "newName")]).unwrap();
        assert_eq!(out, "let s = \"😀\"; newName();\n");
    }

    #[test]
    fn apply_text_edits_handles_bmp_characters() {
        // Accents and CJK are one UTF-16 unit each, so these offsets agree
        // under either interpretation — a guard that the fix didn't break
        // the majority case it used to get right.
        let content = "let café = 1; let oldName = 2;\n";
        let out = apply_text_edits(content, &[edit(0, 18, 0, 25, "newName")]).unwrap();
        assert_eq!(out, "let café = 1; let newName = 2;\n");
    }

    #[test]
    fn apply_text_edits_multiline_splice_uses_utf16_offsets_on_both_ends() {
        let content = "let a = \"😀\"; start\nmiddle\nend \"😀\" tail\n";
        // Start col 14 is just past the emoji on line 0. On line 2,
        // `end "😀" tail`, the emoji occupies UTF-16 columns 5-6, so
        // column 8 is the space before `tail` and column 9 is its `t`.
        let out = apply_text_edits(content, &[edit(0, 14, 2, 8, "X")]).unwrap();
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
        let out = apply_text_edits(content, &edits).unwrap();
        assert_eq!(out, "fn say_hi() {}\n\nfn call() {\n    say_hi();\n}\n");
    }

    #[test]
    fn apply_text_edits_multiline_range_splices_correctly() {
        // Range starts right after "(" on line 0 and ends right before ")"
        // on line 2, so both parens are already outside the edit range —
        // new_text only needs to replace the parameter list between them.
        let content = "fn greet(\n    name: &str\n) {}\n";
        let out = apply_text_edits(content, &[edit(0, 9, 2, 0, "")]).unwrap();
        assert_eq!(out, "fn greet() {}\n");
    }

    #[test]
    fn apply_text_edits_refuses_an_out_of_range_line_instead_of_skipping_it() {
        // A stale WorkspaceEdit computed against content that has since
        // shrunk. Skipping that one edit while applying the rest used to
        // leave a half-renamed file behind.
        let content = "fn greet() {}\n";
        let err = apply_text_edits(
            content,
            &[edit(0, 3, 0, 8, "say_hi"), edit(50, 0, 50, 5, "x")],
        )
        .unwrap_err();
        assert!(err.to_string().contains("line 51"), "{err}");
        // A multi-line edit whose *end* is out of range, likewise.
        assert!(apply_text_edits(content, &[edit(0, 0, 9, 0, "")]).is_err());
    }

    #[test]
    fn apply_text_edits_allows_the_position_after_a_trailing_newline() {
        let out = apply_text_edits("a\n", &[edit(1, 0, 1, 0, "b\n")]).unwrap();
        assert_eq!(out, "a\nb\n");
    }

    #[test]
    fn apply_text_edits_refuses_overlapping_edits() {
        let err = apply_text_edits("abcdef\n", &[edit(0, 0, 0, 4, "X"), edit(0, 2, 0, 6, "Y")])
            .unwrap_err();
        assert!(err.to_string().contains("overlapping"), "{err}");
    }

    #[test]
    fn a_whole_file_edit_may_end_at_the_line_after_the_last() {
        let out = apply_text_edits("a\nb", &[edit(0, 0, 2, 0, "X")]).unwrap();
        assert_eq!(out, "X");
    }

    #[test]
    fn the_out_of_range_error_counts_real_lines() {
        let err = apply_text_edits("a\n", &[edit(5, 0, 5, 1, "x")]).unwrap_err();
        assert!(err.to_string().contains("has 1 line(s)"), "{err}");
    }

    #[test]
    fn apply_text_edits_refuses_a_backwards_range() {
        assert!(apply_text_edits("abcdef\n", &[edit(0, 4, 0, 1, "X")]).is_err());
    }

    #[test]
    fn same_position_inserts_are_applied_in_the_order_given() {
        let out = apply_text_edits(
            "fn f() {}\n",
            &[edit(0, 0, 0, 0, "A"), edit(0, 0, 0, 0, "B")],
        )
        .unwrap();
        assert_eq!(out, "ABfn f() {}\n");
    }

    #[test]
    fn adjacent_edits_both_apply() {
        let out = apply_text_edits(
            "oldold\n",
            &[edit(0, 3, 0, 6, "new"), edit(0, 0, 0, 3, "new")],
        )
        .unwrap();
        assert_eq!(out, "newnew\n");
    }

    #[test]
    fn a_column_past_the_line_end_clamps_before_a_crlf_line_ending() {
        // Per the spec the line ending isn't part of the line, so an
        // overlong character means "end of the text", not "after the \r".
        let out = apply_text_edits("abc\r\ndef\r\n", &[edit(0, 1, 0, 99, "Z")]).unwrap();
        assert_eq!(out, "aZ\r\ndef\r\n");
    }

    #[test]
    fn collect_edits_merges_repeated_entries_for_one_file() {
        let part = |e: TextEdit| {
            DocumentChangeOp::Edit(TextDocumentEdit {
                text_document: VersionedTextDocumentIdentifier {
                    uri: "file:///a.rs".into(),
                },
                edits: vec![e],
            })
        };
        let we = WorkspaceEdit {
            changes: None,
            document_changes: Some(vec![
                part(edit(0, 0, 0, 1, "x")),
                part(edit(1, 0, 1, 1, "y")),
            ]),
        };
        let (files, _) = collect_edits(&we);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1.len(), 2);
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
