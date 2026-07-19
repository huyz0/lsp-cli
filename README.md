# lsp-cli

A command-line tool that gives coding agents (and humans) compiler-accurate
code navigation — go-to-definition, find-references, call hierarchy,
diagnostics, symbol outlines — by talking to real language servers over the
Language Server Protocol, the same protocol your editor uses. No LSP server
installed yet? It installs one for you. No server available at all? It falls
back to a self-built search index so you're never stuck.

Built for agentic coding: every command is scriptable, outputs JSON by
default, and ships with an [agent skill](#installing-the-skill) that teaches
LLMs how to use it well.

## Why

Reading whole files and grepping for strings works, but it's slow and often
wrong — a grep for `User` doesn't know the difference between the class
definition, an import, and an unrelated variable with the same name. This
tool asks the actual language server instead, so "where is this defined,"
"what calls this," and "does this still compile" get real, structurally
correct answers.

## Installation

Linux and macOS only for now — the background daemon (used by every
navigation command for warm server reuse, see below) talks over a Unix
Domain Socket, and there's no Windows named-pipe transport built yet.

**Homebrew:**

```bash
brew install huyz0/tap/lsp-cli
```

**No package manager:**

```bash
curl -fsSL https://raw.githubusercontent.com/huyz0/lsp-cli-rust/main/install.sh | sh
```

Or grab a prebuilt binary directly from
[Releases](https://github.com/huyz0/lsp-cli-rust/releases) — each release
includes a `checksums.txt` so you can verify what you downloaded
(`sha256sum -c checksums.txt`) before running it.

Alternatively, build from source (requires a
[Rust toolchain](https://rustup.rs)):

```bash
git clone https://github.com/huyz0/lsp-cli-rust
cd lsp-cli-rust
cargo build --release
./target/release/lsp --help
```

Put `target/release/lsp` on your `PATH` (or symlink it) so it's just `lsp`.

Language servers are a separate concern — see [Supported languages](#supported-languages)
below. Most install themselves automatically the first time you use them.

## Quick start

```bash
lsp outline src/models.ts                    # file structure, no full read needed
lsp definition src/service.ts --scope createUser
lsp reference src/models.ts --scope User
lsp calls src/service.ts --scope createUser   # who calls this / what does this call
lsp diagnostics src/service.ts                # compiler/type errors
lsp search "User" --kinds class               # find a symbol workspace-wide
```

Every command supports `--output json` (default) or `--output markdown`,
and `--dry-run` to preview the LSP request without sending it. Run
`lsp <command> --help` for full flag documentation, or `lsp schema
[command]` to get a machine-readable JSON Schema of any command's input.

A language server starts automatically on first use and stays warm in a
background daemon, reused across calls — you don't need to manage it
yourself. `lsp server list` shows what's running.

## Supported languages

| Language | Auto-install | Notes |
|---|---|---|
| TypeScript / JavaScript | ✅ | Full support (outline, definition, reference, doc, symbol, calls, diagnostics, search). |
| Python | ✅ (basedpyright) | Full support. |
| Go | ✅ (`go install`) | Full support. |
| Rust | ✅ (GitHub release) | Full support. |
| Java | ✅ (Eclipse jdtls release) | Requires a JDK already present (via [sdkman](https://sdkman.io), `$JAVA_HOME`, or `java` on `PATH`) — this tool won't install a JDK for you. |
| Kotlin | ✅ (GitHub release) | Full support. |
| CSS / JSON | ✅ | Full support. |
| HTML | ✅ | Outline is limited — the server returns a flat symbol list this tool doesn't parse into an outline tree yet. |
| C / C++ | ✅ (`clangd`, GitHub release) | Full support. |
| Lua | ✅ (GitHub release) | Full support. |
| Zig | ✅ (`zls`, GitHub release) | Full support. |
| Bash / shell | ✅ (npm) | Everything except outline — `bash-language-server`'s document-symbol support is minimal and returns nothing for typical scripts; definition/reference/doc all work. |
| C# | ✅ (`csharp-ls` via `dotnet tool install`) | Full support. Requires the .NET SDK on `PATH`. |
| Ruby | ✅ (`ruby-lsp` via `gem install`) | Full support, verified live. `ruby-lsp` composes a Bundler-managed bundle on startup, which needs a working user-writable Bundler/gem setup (and `libyaml-dev` for the `psych` gem) — see [CONTRIBUTING.md](CONTRIBUTING.md#ruby-ruby-lsp) for the one-time host setup. |
| Deno | Detected on `PATH`, not installed | Full support once `deno` is on `PATH` — install it yourself from [deno.land](https://deno.land). |

**"TypeScript" and "Deno" aren't two languages** — both serve the same
`.ts`/`.tsx`/`.js`/`.jsx` files. They're two different toolchains for
that one language, each needing its own language server: "TypeScript"
means `typescript-language-server` (Node-style resolution via
`node_modules`, npm packages), and "Deno" means Deno's own built-in `deno
lsp` (URL/JSR-style imports, no `node_modules`). Which one a project gets
is decided by the nearest `deno.json`/`deno.jsonc` (→ Deno) vs.
`package.json`/`tsconfig.json`/`jsconfig.json` (→ TypeScript) — see
[docs/language-support.md](docs/language-support.md#typescript-vs-deno)
for the detection order when a project has both.

Check what's installed with `lsp install --list`. Install or update one
explicitly with `lsp install <language>` / `lsp install <language> --update`,
or everything with `lsp install --all`.

## Installing the skill

`skills/lsp-code-analysis/SKILL.md` documents this CLI for an LLM/agent
(Claude Code, Cursor, etc): command reference, the `--scope`/`--find`
location syntax, pagination, recommended workflows, and troubleshooting.
Install it with [vercel-labs/skills](https://github.com/vercel-labs/skills):

```bash
npx skills add <path-or-url-to-this-repo> -a claude-code
```

The CLI auto-discovers `SKILL.md` files under a repo's `skills/` directory,
so pointing it at this repo's root is enough — no extra path argument
needed. To install manually instead, copy `skills/lsp-code-analysis/SKILL.md`
into whichever `.claude/skills/<name>/` (or equivalent) directory your agent
tooling reads skills from.

## Configuration

`~/.lsp-cli/config.json` (optional, created by you — missing or malformed
files just fall back to defaults):

```json
{
  "idleTimeout": 600000,
  "managerTimeout": 60000,
  "defaultMaxItems": 20
}
```

- `idleTimeout` (ms, default 10 minutes) — how long a warm language server
  sits idle before the background daemon shuts it down.
- `managerTimeout` (ms, default 60s) — daemon request timeout.
- `defaultMaxItems` — default page size for paginated commands
  (`reference`, `search`).

## MCP server mode

`lsp mcp` runs this CLI as an [MCP](https://modelcontextprotocol.io) server
over stdio, so its commands are callable as MCP tools instead of shell
invocations.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and running tests,
[docs/architecture.md](docs/architecture.md) for how the codebase is
organized, [docs/language-support.md](docs/language-support.md) for
per-language status detail, and [docs/RELEASING.md](docs/RELEASING.md) for
how releases and package-manager publishing work.
