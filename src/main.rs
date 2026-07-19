// `daemon.rs`/`manager_client.rs` use `tokio::net::UnixListener`/`UnixStream`
// unconditionally with no `#[cfg(unix)]` gating, and those types simply
// don't exist in tokio's Windows build — without this, attempting a
// non-Unix build fails with a wall of cryptic "unresolved import"/"no
// method named `bind`" errors scattered across unrelated-looking files,
// instead of one clear message explaining why up front. The daemon (and
// therefore all warm-server reuse — every navigation command routes
// through it, see README "Warm server reuse for navigation commands") has
// no Windows implementation yet: that would mean a named-pipe transport
// (`tokio::net::windows::named_pipe`) alongside the Unix socket one, which
// hasn't been built. See README "Deviations from the TypeScript original".
#[cfg(not(unix))]
compile_error!(
    "lsp-cli-rust's background daemon (used by every navigation command for warm server reuse) is Unix-only today — it uses a Unix Domain Socket with no Windows named-pipe equivalent implemented yet. See README.md's \"No Windows named-pipe support\" note."
);

mod bm25;
mod commands;
mod config;
mod daemon;
mod format;
mod install;
mod locate;
mod lsp_client;
mod manager_client;
mod mcp;
mod project;
mod protocol;
mod registry;
mod schema;
mod watcher;

use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::ScopeFind;
use format::OutputFormat;

#[derive(Parser)]
#[command(
    name = "lsp",
    version = "0.1.0",
    about = "LSP-backed code navigation CLI"
)]
struct Cli {
    /// Internal flag: run as the background manager daemon.
    #[arg(long, hide = true)]
    daemon: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

/// Shared help text for the `--scope`/`--find` location syntax used by
/// every navigation command below (definition, reference, doc, symbol,
/// outline, locate). Run `lsp locate <file> --scope ... --find ...` first
/// to verify a scope+find pattern resolves to the position you expect,
/// before spending an LSP round trip on it.
const SCOPE_HELP: &str = "Location within the file: a line number (`42`), a line range (`10,20`), or a symbol path (`MyClass` or `MyClass.method`). Combine with --find to pick an exact position inside that scope.";
const FIND_HELP: &str = "Text pattern to locate within --scope's range, whitespace-insensitive. Mark the exact cursor position with <|>, e.g. --find \"<|>createUser\" or --find \"return <|>result\". If omitted, the position defaults to the start of --scope.";
const OUTPUT_HELP: &str = "Output format: `json` (default, for agents) or `markdown` (for humans).";
const PROJECT_HELP: &str = "Override the auto-detected project root (normally found by walking up from <file> to the nearest package.json/Cargo.toml/go.mod/etc).";
const DRY_RUN_HELP: &str =
    "Print the LSP request that would be sent, without contacting a language server.";

#[derive(Subcommand)]
enum Commands {
    /// Show file structure (classes, functions, methods)
    ///
    /// Call this first when exploring an unfamiliar file — cheaper than
    /// reading the whole file, and gives you symbol names to pass as
    /// --scope to other commands.
    #[command(alias = "o")]
    Outline {
        file: String,
        /// Include all symbols (fields, parameters, variables), not just
        /// top-level class/interface/enum/function/module/namespace/struct.
        #[arg(long)]
        all: bool,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
    },
    /// Navigate to where a symbol is defined
    ///
    /// Requires --scope to select the symbol (e.g. --scope createUser, or
    /// --scope 12 --find "<|>User" for a precise position). Run `lsp
    /// locate` first if unsure the scope+find resolves correctly.
    #[command(alias = "def")]
    Definition {
        file: String,
        /// One of: definition (default), declaration, type_definition.
        #[arg(long, default_value = "definition")]
        mode: String,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
    },
    /// Find all usages of a symbol across the workspace
    ///
    /// Requires --scope to select the symbol. Results are paginated — see
    /// --max-items.
    #[command(alias = "ref")]
    Reference {
        file: String,
        /// One of: references (default), implementations.
        #[arg(long, default_value = "references")]
        mode: String,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
        /// Maximum results to return in this call.
        #[arg(long, default_value = "20")]
        max_items: usize,
        /// 0-based offset into the full result set — pass the previous
        /// page's item count to fetch the next page.
        #[arg(long, default_value = "0")]
        start_index: usize,
        /// Session ID for stable pagination. Note: since each CLI invocation
        /// is a fresh process (no persistent daemon-backed request cache in
        /// this port — matches the TS CLI's own per-process Map), this is
        /// accepted for interface compatibility but each call still queries
        /// the LSP server fresh.
        #[arg(long)]
        pagination_id: Option<String>,
    },
    /// View the type signature and documentation (hover text) for a symbol
    ///
    /// Requires --scope to select the symbol.
    #[command(alias = "d")]
    Doc {
        file: String,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
    },
    /// Report compiler/type-checker errors and warnings for a file
    ///
    /// Uses LSP pull diagnostics (textDocument/diagnostic). Run this after
    /// editing a file to check it still compiles/typechecks without
    /// invoking the project's own build tool. Not every language server
    /// supports pull diagnostics yet — if it fails, the error message says
    /// so explicitly rather than silently returning no diagnostics.
    #[command(alias = "diag")]
    Diagnostics {
        file: String,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
    },
    /// Find who calls, or is called by, a symbol
    ///
    /// Requires --scope to select the symbol. Uses LSP call hierarchy
    /// (textDocument/prepareCallHierarchy + callHierarchy/incoming|
    /// outgoingCalls), which is more precise than `reference` for "what
    /// breaks if I change this" — it only follows actual call sites, not
    /// every textual usage (imports, type annotations, reads/writes).
    #[command(alias = "c")]
    Calls {
        file: String,
        /// One of: incoming (default — who calls this), outgoing (what this calls).
        #[arg(long, default_value = "incoming")]
        direction: String,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
    },
    /// Get the full source code of the symbol at a location
    ///
    /// Prefer this over reading the whole file when you only need one
    /// function/class — requires --scope to select the symbol.
    #[command(alias = "sym")]
    Symbol {
        file: String,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
    },
    /// Verify and resolve a scope+find location in a file (no LSP server needed)
    ///
    /// Use this to check a --scope/--find pair resolves to the position
    /// you expect before spending an LSP round trip on
    /// definition/reference/doc/symbol.
    #[command(alias = "l")]
    Locate {
        file: String,
        #[arg(long, help = SCOPE_HELP)]
        scope: Option<String>,
        #[arg(long, help = FIND_HELP)]
        find: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
    },
    /// Search for symbols by name across the workspace
    ///
    /// Uses LSP workspace/symbol, falling back to a self-built BM25 index
    /// when no server is available. Use this to find a symbol when you
    /// don't already know which file it's in.
    #[command(alias = "s")]
    Search {
        query: String,
        /// Filter by symbol kind(s), e.g. --kinds function --kinds class.
        /// Values: class, interface, function, method, variable, constant,
        /// enum, struct.
        #[arg(long)]
        kinds: Vec<String>,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
        #[arg(long, help = DRY_RUN_HELP)]
        dry_run: bool,
        /// Maximum results to return in this call.
        #[arg(long, default_value = "20")]
        max_items: usize,
        /// 0-based offset into the full result set — pass the previous
        /// page's item count to fetch the next page.
        #[arg(long, default_value = "0")]
        start_index: usize,
        /// Session ID for stable pagination (see the --pagination-id note
        /// on `reference`; accepted but not required for correctness).
        #[arg(long)]
        pagination_id: Option<String>,
    },
    /// Install or update a language server
    ///
    /// Auto-install already happens on first use of a navigation command —
    /// call this explicitly only to pre-install, force an update, or check
    /// install status with --list.
    #[command(alias = "i")]
    Install {
        /// Language to install, or "all". See --list for supported values
        /// and current install status.
        language: Option<String>,
        /// Install every supported language server.
        #[arg(long)]
        all: bool,
        /// Show install status for every supported language instead of installing.
        #[arg(long)]
        list: bool,
        /// Reinstall/update even if already installed.
        #[arg(long)]
        update: bool,
    },
    /// Manage background LSP server processes
    ///
    /// Navigation commands already auto-start and reuse warm servers — you
    /// don't need to call `start` manually. Useful for inspecting what's
    /// running (`list`) or forcing a clean restart (`stop`/`shutdown`)
    /// after a server misbehaves.
    #[command(alias = "srv")]
    Server {
        /// One of: list (default), start, stop, shutdown.
        subcommand: Option<String>,
        /// Project file or directory (for start/stop). Defaults to the
        /// current directory for `start` if omitted.
        path: Option<String>,
        /// With `stop`: stop every running server, not just the one for --path.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "json", help = OUTPUT_HELP)]
        output: String,
    },
    /// Start an MCP (Model Context Protocol) server (stdio transport only)
    ///
    /// Makes this CLI's commands callable as MCP tools instead of shell
    /// invocations.
    Mcp {
        /// Only "stdio" is implemented in this Rust port.
        #[arg(long, default_value = "stdio")]
        transport: String,
        #[arg(short, long, help = PROJECT_HELP)]
        project: Option<String>,
    },
    /// Dump JSON schema for CLI commands
    ///
    /// Shows one command's input schema if given, or lists every command's
    /// schema otherwise. Prefer this over reading docs to learn a
    /// command's exact argument shape programmatically.
    Schema {
        /// Command name to show the schema for, e.g. `lsp schema definition`.
        command: Option<String>,
    },
}

fn fmt_of(output: &str) -> Result<OutputFormat> {
    match output {
        "json" => Ok(OutputFormat::Json),
        "markdown" => Ok(OutputFormat::Markdown),
        other => anyhow::bail!("Unknown --output value: {other} (expected one of: json, markdown)"),
    }
}

/// Supports `--json '{"key":"value"}'` style invocation, matching
/// index.ts's injectJsonArgs(). Used by the MCP tool-call bridge and by
/// anyone who wants to pipe structured input instead of flags.
fn inject_json_args(mut argv: Vec<String>) -> Vec<String> {
    if argv.iter().any(|a| a == "mcp") || argv.iter().any(|a| a == "--daemon") {
        return argv;
    }
    let Some(idx) = argv.iter().position(|a| a == "--json") else {
        return argv;
    };
    if idx + 1 >= argv.len() {
        return argv;
    }
    let payload = argv[idx + 1].clone();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else {
        return argv;
    };
    argv.drain(idx..=idx + 1);

    let Some(obj) = v.as_object() else {
        return argv;
    };
    let positional_keys = ["command", "subcommand", "file", "query", "language", "path"];
    for key in positional_keys {
        if let Some(val) = obj.get(key).and_then(|v| v.as_str()) {
            argv.push(val.to_string());
        }
    }
    for (k, val) in obj {
        if positional_keys.contains(&k.as_str()) {
            continue;
        }
        match val {
            serde_json::Value::Bool(true) => argv.push(format!("--{k}")),
            serde_json::Value::Bool(false) => {}
            serde_json::Value::Array(items) => {
                for item in items {
                    argv.push(format!("--{k}"));
                    argv.push(
                        item.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| item.to_string()),
                    );
                }
            }
            serde_json::Value::String(s) => {
                argv.push(format!("--{k}"));
                argv.push(s.clone());
            }
            other => {
                argv.push(format!("--{k}"));
                argv.push(other.to_string());
            }
        }
    }
    argv.push("--output".to_string());
    argv.push("json".to_string());
    argv
}

#[tokio::main]
async fn main() {
    let raw_argv: Vec<String> = std::env::args().collect();

    if raw_argv.iter().any(|a| a == "--daemon") {
        if let Err(e) = daemon::start_daemon().await {
            eprintln!("lsp: daemon error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let argv = inject_json_args(raw_argv);
    let cli = Cli::parse_from(argv);

    if let Err(e) = run(cli).await {
        eprintln!("lsp: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let Some(cmd) = cli.command else {
        anyhow::bail!("no command given (try --help)");
    };

    match cmd {
        Commands::Outline {
            file,
            all,
            scope: _,
            find: _,
            project,
            output,
            dry_run,
        } => {
            commands::run_outline(&file, all, project.as_deref(), dry_run, &fmt_of(&output)?)
                .await?;
        }
        Commands::Definition {
            file,
            mode,
            scope,
            find,
            project,
            output,
            dry_run,
        } => {
            commands::run_definition(
                &file,
                ScopeFind { scope, find },
                &mode,
                project.as_deref(),
                dry_run,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Reference {
            file,
            mode,
            scope,
            find,
            project,
            output,
            dry_run,
            max_items,
            start_index,
            pagination_id: _,
        } => {
            commands::run_reference(
                &file,
                ScopeFind { scope, find },
                &mode,
                project.as_deref(),
                dry_run,
                max_items,
                start_index,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Doc {
            file,
            scope,
            find,
            project,
            output,
            dry_run,
        } => {
            commands::run_doc(
                &file,
                ScopeFind { scope, find },
                project.as_deref(),
                dry_run,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Diagnostics {
            file,
            project,
            output,
            dry_run,
        } => {
            commands::run_diagnostics(&file, project.as_deref(), dry_run, &fmt_of(&output)?)
                .await?;
        }
        Commands::Calls {
            file,
            direction,
            scope,
            find,
            project,
            output,
            dry_run,
        } => {
            commands::run_calls(
                &file,
                ScopeFind { scope, find },
                &direction,
                project.as_deref(),
                dry_run,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Symbol {
            file,
            scope,
            find,
            project,
            output,
            dry_run,
        } => {
            commands::run_symbol(
                &file,
                ScopeFind { scope, find },
                project.as_deref(),
                dry_run,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Locate {
            file,
            scope,
            find,
            output,
        } => {
            commands::run_locate(&file, ScopeFind { scope, find }, &fmt_of(&output)?)?;
        }
        Commands::Search {
            query,
            kinds,
            project,
            output,
            dry_run,
            max_items,
            start_index,
            pagination_id: _,
        } => {
            let kinds = if kinds.is_empty() { None } else { Some(kinds) };
            commands::run_search(
                &query,
                kinds,
                project.as_deref(),
                dry_run,
                max_items,
                start_index,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Install {
            language,
            all,
            list,
            update,
        } => {
            if list || (language.is_none() && !all) {
                install::run_install_list()?;
            } else {
                install::run_install(
                    if all {
                        "all"
                    } else {
                        language.as_deref().unwrap_or("")
                    },
                    update,
                )
                .await?;
            }
        }
        Commands::Server {
            subcommand,
            path,
            all,
            output,
        } => {
            run_server(
                subcommand.as_deref().unwrap_or("list"),
                path.as_deref(),
                all,
                &fmt_of(&output)?,
            )
            .await?;
        }
        Commands::Mcp { transport, project } => {
            if transport == "stdio" {
                mcp::run_mcp_stdio(project.as_deref())?;
            } else {
                anyhow::bail!(
                    "Only the stdio MCP transport is implemented in this Rust port (see README)."
                );
            }
        }
        Commands::Schema { command } => {
            commands::run_schema(command.as_deref())?;
        }
    }
    Ok(())
}

async fn run_server(sub: &str, path: Option<&str>, all: bool, fmt: &OutputFormat) -> Result<()> {
    let client = manager_client::ManagerClient::new();
    match sub {
        "list" => {
            client.ensure_running().await?;
            let servers = client.list_servers().await?;
            match fmt {
                OutputFormat::Json => println!(
                    "{}",
                    serde_json::json!({ "kind": "serverList", "servers": servers })
                ),
                OutputFormat::Markdown => {
                    if servers.is_empty() {
                        println!("No servers running.");
                    } else {
                        for s in servers {
                            println!("{:<12} {:<10} {}", s.language, s.status, s.project_root);
                        }
                    }
                }
            }
        }
        "start" => {
            client.ensure_running().await?;
            let target = match path {
                Some(p) => p.to_string(),
                None => std::env::current_dir()
                    .map_err(|e| anyhow::anyhow!("cannot determine current directory: {e}"))?
                    .to_string_lossy()
                    .to_string(),
            };
            let info = client.create_server(&target).await?;
            println!("Started {} server for {}", info.language, info.project_root);
        }
        "stop" => {
            client.ensure_running().await?;
            let stopped = client.delete_servers(path, all).await?;
            if stopped.is_empty() {
                println!("No servers stopped.");
            } else {
                for s in stopped {
                    println!("Stopped {} server for {}", s.language, s.project_root);
                }
            }
        }
        "shutdown" => {
            client.shutdown().await?;
            println!("Manager shutdown.");
        }
        other => {
            anyhow::bail!("Unknown server subcommand: {other}\nValid subcommands: list, start, stop, shutdown");
        }
    }
    Ok(())
}
