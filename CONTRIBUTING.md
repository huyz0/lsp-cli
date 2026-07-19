# Contributing

## Setup

Requires a [Rust toolchain](https://rustup.rs).

```bash
cargo build --release
./target/release/lsp --help
cargo test
```

The binary is named `lsp`.

## Running the full test suite

Most tests are gated on a real language server being installed and skip
cleanly (with a message) if it isn't. To exercise the full suite, install
and symlink the servers into `~/.lsp-cli/servers/`:

```bash
NPM=npm  # or your Node package manager
$NPM install -g typescript-language-server basedpyright vscode-langservers-extracted
go install golang.org/x/tools/gopls@latest
rustup component add rust-analyzer

mkdir -p ~/.lsp-cli/servers && cd ~/.lsp-cli/servers
ln -sf "$(which typescript-language-server)" typescript-language-server
ln -sf "$(which basedpyright-langserver)" basedpyright-langserver
ln -sf "$(which gopls || echo ~/go/bin/gopls)" gopls
ln -sf "$(rustup which rust-analyzer)" rust-analyzer
ln -sf "$(which vscode-html-language-server)" vscode-html-language-server
ln -sf "$(which vscode-css-language-server)" vscode-css-language-server
ln -sf "$(which vscode-json-language-server)" vscode-json-language-server

cd <this repo>/tests/fixtures/typescript_project && npm install typescript
```

### Ruby (`ruby-lsp`)

`ruby-lsp` composes its own Bundler-managed bundle on every startup (see
[docs/language-support.md](docs/language-support.md#ruby)), which needs a
working Bundler plus a writable gem/bundle path. The system gem directory
usually isn't writable by a non-root user, and installing a native gem
extension (`psych`) needs the `libyaml` headers. Without root, or with an
unprivileged gem setup, this fails partway through in a way that looks like
a broken `lsp-cli-rust` integration but isn't:

```bash
sudo apt install -y libyaml-dev   # or your distro's libyaml headers package

gem install bundler --user-install
gem install ruby-lsp --user-install --no-document --bindir ~/.lsp-cli/servers

# Both need to be set wherever the lsp daemon process is started from:
# PATH so ruby-lsp can find `bundle`, BUNDLE_PATH so the composed bundle
# it builds per-project doesn't try to write into the (likely read-only)
# system gem directory.
export PATH="$(gem environment | grep 'USER INSTALLATION DIRECTORY' | sed 's/.*: //')/bin:$PATH"
export BUNDLE_PATH="$HOME/.local/share/gem/bundle"
```

Some tests spawn concurrent language-server processes against the same
fixture project and can interfere with each other under default parallel
test execution, or under general system load (inherent to per-invocation
`tsserver`/`rust-analyzer` cold-start timing); run `cargo test --
--test-threads=1` if you see spurious failures there.

For details on which language server backs which support level, see
[docs/language-support.md](docs/language-support.md). For how the codebase
is organized, see [docs/architecture.md](docs/architecture.md).

## Test suite structure

`cargo test` runs 45 unit tests plus 67 integration tests across 21 files
under `tests/`, against fixture projects in `tests/fixtures/`:

- **Unit** (in `src/`): Content-Length framing/parsing (`lsp_client.rs`), BM25
  tokenization/ranking *and* the per-language `extract_symbols` regex
  heuristics for TS/Python/Go/Rust/Java (`bm25.rs`), locate scope/find
  resolution including the `<|>` cursor marker and whitespace-insensitive
  matching (`locate.rs`), language/project-root detection (`registry.rs`),
  config defaults *and* `load_config`'s actual file-reading/merging logic
  against real tempfiles (`config.rs`), formatter output shape
  (`format.rs`), and install.rs's pure logic: every rust-analyzer OS/arch
  release-triple mapping plus the unsupported-platform error path, the
  node-wrapper shell script generation, and npm-spec-table invariants
  (`install.rs`).
- **Integration** (`tests/*.rs`, spawning the built binary via
  `CARGO_BIN_EXE_lsp`): `help`, `schema`, `locate` (no LSP server needed,
  always run); `outline`/`definition`/`reference`/`doc`/`symbol`/`calls`/
  `diagnostics`/`search` for TypeScript; `python.rs`, `go_lang.rs`,
  `rust_lang.rs`, `web.rs` (CSS/JSON/HTML), `java_kotlin.rs`,
  `markdown_lang.rs` for the other languages (each skips with a message if
  its server isn't installed, see
  [docs/language-support.md](docs/language-support.md) for what's actually
  installed and passing in a given environment). `mcp_stdio.rs` speaks the
  JSON-RPC protocol directly over the child process's stdio (`initialize`,
  `tools/list`, `tools/call`, unknown-tool error).
- **`server.rs`** exercises the real background daemon lifecycle. No LSP
  server needed for list/stop/shutdown against an empty/no-daemon state;
  3 more (gated on `has_ts_server()`) cover daemon concurrency:
  `kill_and_reload_respawns_a_dead_server` (kills a real child PID via
  `kill -9`, asserts the next `server start` produces a different PID),
  `reusing_a_running_server_refreshes_idle_since` (asserts `idle_since`
  advances across reuse instead of staying frozen at creation time), and
  `concurrent_server_start_for_the_same_project_creates_only_one_entry`
  (fires 4 concurrent `server start` calls for the same project and
  asserts exactly 1 tracked entry survives).
- **`watcher.rs`** covers file-watching end-to-end: spawns the daemon
  directly (so its stderr is observable, unlike the normal
  `ensure_running`-spawned path which discards it), starts a real
  TypeScript server, edits a file the server never opened, and asserts the
  watcher's `[watcher] ... change(s) detected` line appears on stderr
  within 5s.
