# Architecture

See [../CONTRIBUTING.md](../CONTRIBUTING.md) for setup and testing, and
[language-support.md](language-support.md) for per-language status.

## Modules

- **CLI plumbing** (`src/main.rs`): clap-derive command tree, including
  short aliases (`o`, `def`, `ref`, `d`, `sym`, `l`, `s`, `i`, `srv`),
  pagination flags, and a `--json '{...}'` structured-input path (parses a
  JSON blob into flags instead of requiring individual `--flag value`
  arguments; useful for programmatic callers, e.g. the MCP bridge).
- **BM25 search** (`src/bm25.rs`): a from-scratch Okapi BM25 implementation
  (k1=1.5, b=0.75) with camelCase/snake_case tokenization, prefix-match
  scoring for partial identifiers, and its own symbol extractor (regex
  heuristics covering every language in [language-support.md](language-support.md):
  TS/JS/Deno, Python, Go, Rust, Java/Kotlin, C/C++, Lua, Zig, Ruby, C#,
  Bash, plus lighter-weight extraction for CSS selectors, JSON keys, and
  HTML element ids) that walks the project tree and indexes
  classes/functions/methods/selectors/keys. This is the
  fallback path for `search` whenever no LSP server answers
  `workspace/symbol` (in practice, whenever no language server binary is
  installed), so it's the primary functional path for anyone without local
  LSP servers.
- **LSP JSON-RPC client** (`src/lsp_client.rs`): hand-rolled Content-Length
  framing, `initialize`, `textDocument/documentSymbol`,
  `textDocument/definition`/`declaration`/`typeDefinition`,
  `textDocument/references`/`implementation`, `textDocument/hover`,
  `textDocument/diagnostic`, call hierarchy, `workspace/symbol`,
  `shutdown`/`exit`. Framing and message parsing are unit tested. Answers
  server-initiated requests (e.g. `workspace/configuration`) with a minimal
  default response, since some servers (rust-analyzer) stall otherwise;
  retries the spec-defined `ContentModified` (-32801) error with backoff,
  matched via a typed `RpcError` enum rather than string-matching, and enforces
  an absolute 120s wall-clock deadline per request (independent of the
  per-message idle timeout, which resets on every notification and so
  can't alone catch a chatty-but-stuck server); and spawns child processes
  with `.kill_on_drop(true)` as a guaranteed cleanup backstop regardless of
  code path.
- **Locate resolver** (`src/locate.rs`): the `--scope`/`--find` location
  syntax (line numbers, line ranges, dotted symbol paths, `<|>` cursor
  marker, whitespace-insensitive matching), with unit tests.
- **Language registry** (`src/registry.rs`): extension → language, project
  root marker detection (walks up the directory tree, canonicalizing the
  containing directory so the same project always resolves to the same
  `project_root` key regardless of which code path detected it: a bare
  directory passed to `server start` and a concrete file passed to a
  navigation command must agree), and the language → server-binary/args
  mapping for every supported language.
- **Formatters** (`src/format.rs`): JSON and Markdown output for every
  command (1-based line numbers, consistent JSON keys, consistent icons in
  Markdown output).
- **Config loading** (`src/config.rs`): reads `~/.lsp-cli/config.json`,
  merges over defaults, never errors on a missing/malformed file.
- **MCP mode** (`src/mcp.rs`): a stdio JSON-RPC loop implementing
  `initialize`, `tools/list`, and `tools/call`. Tool calls shell out to the
  same binary with `--json` + `--project`.
- **Schema dump** (`src/schema.rs`): `lsp schema [command]`.

## Manager daemon (`src/daemon.rs`)

A background process listening on a Unix Domain Socket, built with axum +
hyper's low-level server builder (axum's `Router` doesn't yet have
first-class `UnixListener` support, so the connection loop is wired
manually with `hyper_util::server::conn::auto`). Supports `/list`,
`/create`, `/delete`, `/request`, `/notify`, `/shutdown`; spawns and
initializes real `LspClient` instances per project root+language, and
reaps idle servers on a timer using the configured `idleTimeout`. `lsp
server list/start/stop/shutdown` talks to it through
`src/manager_client.rs`, a minimal hand-rolled HTTP/1.1 client over the
socket. Also watches each project root for file changes and pushes
debounced `workspace/didChangeWatchedFiles` notifications to live servers.
See `src/watcher.rs`.

`create()` (spawning a server for a project) is guarded by a
per-project-root+language lock, not a single global lock, so starting a
server for one project never blocks starting a server for an unrelated
one. Within a lock, a cached entry is only reused after confirming the
underlying process is still alive (`LspClient::is_alive()`), so a
crashed/killed server gets evicted and respawned rather than served stale.
Daemon spawn itself is serialized across OS processes via an
atomically-created lock file (`~/.lsp-cli/manager.spawn.lock`, with
stale-lock detection in case a spawner crashed), and `start_daemon()`
connect-checks the socket before touching it, so two processes racing to
cold-start the daemon can't orphan one of them. The Unix socket and its
containing directory are created `0600`/`0700` (owner-only): the daemon
speaks an unauthenticated HTTP API, so this matters on shared/multi-user
hosts.

Every navigation command opens its target file (`textDocument/didOpen`)
before querying; since servers are reused warm across calls, `didOpen` on
an already-open file is turned into a `didChange` instead
(`LspClient::sync_document`), since some servers (typescript-language-server)
reject a duplicate `didOpen` outright and skip reprocessing the file,
which would otherwise silently serve a stale view of it. Proxy calls also
always pass the resolved language explicitly, not just the project root:
languages with no root markers (html/css/json/markdown) fall back
to the file's own directory as `project_root`, so two different-language
servers can share that key and must be disambiguated by language too.

### Warm server reuse

Navigation commands (`outline`/`definition`/`reference`/`doc`/`symbol`/
`calls`/`diagnostics`/`search`) proxy through the daemon
(`commands.rs::ensure_daemon_session` → `ManagerClient::proxy_request`/
`proxy_notify` → `Manager::proxy_request`/`proxy_notify`), so a server
started for a project is reused warm across calls, including across
separate OS processes since the daemon is its own long-lived process.
It's evicted only by `lsp server stop`, an idle timeout, or a detected crash.

A fixed ~3000ms settle delay is paid after every `didOpen`/`didChange`
before issuing the actual request (`commands.rs::DIDOPEN_SETTLE_DELAY_MS`),
since there's no real "server finished indexing" signal to poll instead.
It was tuned against multiple simultaneously-warm servers competing for CPU. A
robust fix would poll an actual readiness signal instead of sleeping a
constant.

## Automatic language server installation (`src/install.rs`)

Every language except `deno` is auto-installable, including Java.
`typescript`/`python`/`html`/`css`/`json`/`markdown` run `npm
install <package>` into `~/.lsp-cli/packages/` and write a `#!/bin/sh`
wrapper into `~/.lsp-cli/servers/` that execs `node <entry> "$@"`; `go`
runs `go install golang.org/x/tools/gopls@latest` into an isolated
`GOPATH` (`~/.lsp-cli/go/`) and symlinks the result in; `rust` and
`kotlin` fetch the latest GitHub Release asset (via `reqwest`), staged in
a fresh per-download temp directory (`create_dir`, which fails rather than
follows a pre-planted symlink at a predictable path) and extracted with
the system `gunzip`/`unzip`; `java` fetches Eclipse's
`jdt-language-server-latest.tar.gz`, extracts it to
`~/.lsp-cli/servers/jdtls-dist/`, and writes a wrapper script pinning in a
JDK it finds via `~/.sdkman/candidates/java/current`, `$JAVA_HOME`, or
`java` on `PATH` (this tool doesn't install a JDK itself, that's a much
bigger, more opinionated dependency than any other managed server, so `lsp
install java` fails with an explicit "no JDK found, try `sdk install
java`" message rather than silently doing nothing). `deno` is the one
truly unmanaged language: `ensure_installed` checks `deno --version` on
`PATH` and uses it if present, but never downloads it. `ensure_installed`
is called on the CLI side before contacting the daemon (so install
progress prints to the user's own terminal rather than the daemon's
normally-discarded stdio), but only when the daemon doesn't already report
a live warm server for that project+language, since otherwise this meant
spawning a `<bin> --version` subprocess on *every single navigation
command* even against an already-warm server.

**Known accepted risk: no checksum/signature verification on downloaded
binaries.** `rust`/`kotlin` (GitHub Releases) and `java` (Eclipse's
`jdt-language-server-latest.tar.gz` (an unversioned "latest" snapshot, not
even a pinned release) are fetched over HTTPS with only an
HTTP-success-status check; the bytes are `chmod +x`'d and later executed
directly as the LSP server process with no checksum or signature
verification against the fetched artifact. This reaches process
execution, unlike the npm path (which at least benefits from the npm
registry's own package integrity mechanisms). Flagged here explicitly
rather than fixed, since a real fix needs per-upstream-project curated
trusted checksums/keys (none of rust-analyzer, kotlin-language-server, or
Eclipse's jdtls snapshots publish a checksum manifest as part of their
release process today).

## Commands beyond core navigation

- **`lsp calls <file> --scope <symbol> [--direction incoming|outgoing]`**:
  LSP call hierarchy (`textDocument/prepareCallHierarchy` +
  `callHierarchy/incomingCalls`/`outgoingCalls`). More precise than
  `reference` for "what breaks if I change this": it only follows actual
  call sites, not every textual usage. `tests/calls.rs`.
- **`lsp diagnostics <file>`**: reports compiler/type-checker errors and
  warnings. Tries LSP 3.17 pull diagnostics (`textDocument/diagnostic`)
  first; if the server doesn't support it (typescript-language-server
  notably doesn't), falls back to whatever `textDocument/publishDiagnostics`
  notifications it's already pushed, captured opportunistically by
  `LspClient` any time a notification is drained. If a server has never
  pushed anything either, the pull failure is surfaced with an explicit
  hint rather than silently returning an empty list. `tests/diagnostics.rs`.

## CLI/agent usability

Every command and flag has a real `--help` description (`src/main.rs`):
what `--scope`/`--find` syntax means, valid `--mode`/`--direction`/
`--output` values, when to run `lsp locate` first. Split into a short
one-line summary for the top-level `lsp --help` list and a longer detail
block for `lsp <command> --help`.

Every mode/direction/output flag fails loudly on an invalid value with the
valid options listed (e.g. `Unknown mode: bogus (expected one of:
references, implementations)`) rather than silently falling back to a
default. That matters for scripted/agent callers, where a silently-wrong
result is worse than a clear error.

`skills/lsp-code-analysis/SKILL.md` teaches an LLM/agent how to use this
CLI well: command reference, `--scope`/`--find` syntax, a troubleshooting
table, and command documentation. See README's "Installing the skill".

## Portability

No Windows named-pipe support. The daemon uses a Unix Domain Socket with no
Windows equivalent implemented (`tokio::net::windows::named_pipe` would be
the path forward). `main.rs` has a `#[cfg(not(unix))] compile_error!`
explaining the limitation up front, rather than a wall of cryptic type
errors from `UnixListener`/`UnixStream` not existing on that target.
