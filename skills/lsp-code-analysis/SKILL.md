---
name: lsp-code-analysis
description: Answers structural questions about a codebase by querying a real language server through the `lsp` CLI - where a symbol is defined, what references or calls it, what a file contains, what a symbol's type and docs are, whether a file still compiles, and renaming a symbol across every file that uses it. Use when the user asks "where is X defined", "who calls X", "find all usages of X", "what's in this file", "rename X everywhere", "does this still compile", or when tracing impact before changing a symbol. Supports TypeScript/JavaScript, Deno, Python, Go, Rust, Java, Kotlin, C/C++, C#, Ruby, Lua, Zig, Bash, HTML, CSS and JSON.
---

# LSP code analysis

The `lsp` CLI answers questions about code by asking a language server,
so results are based on what the compiler resolves rather than on text
matching. A grep for `User` cannot tell a class from an import from an
unrelated local; `lsp definition` can.

Linux and macOS only. If `lsp --version` fails, the tool is not available
here and everything below is moot.

## When to use it, and when not to

Reach for `lsp` when the question is about a **symbol**:

| Question | Command |
|---|---|
| What's in this file? | `lsp outline <file>` |
| Where is X defined? | `lsp definition <file> --scope X` |
| What uses X? | `lsp reference <file> --scope X` |
| What calls X? (call sites only) | `lsp calls <file> --scope X` |
| What extends / implements X? | `lsp hierarchy <file> --scope X` (gopls, csharp-ls, jdtls), or `lsp reference <file> --scope X --mode implementations` (any server) |
| What is X's type and docs? | `lsp doc <file> --scope X` |
| Show me X's source | `lsp symbol <file> --scope X` |
| Does this file compile? | `lsp diagnostics <file>` |
| Where is X, anywhere? | `lsp search "X"` |
| Rename X everywhere | `lsp rename <file> --scope X --new-name Y` |

The file-scoped commands all need a file path. When you do not have one
yet, start with `lsp search` to find the symbol, then use the file it
reports.

Use grep or read instead when:

- You are looking for **text**, not a symbol: a string literal, a comment,
  a config value, a log line, prose in a README.
- You want **every textual occurrence**, including comments and strings.
  `reference` deliberately returns only resolved usages.
- The file is small and you want all of it anyway. `outline` plus a
  couple of `symbol` calls costs more than one read of a 100-line file.
- The language is not in the list in the frontmatter.

**Warm calls are fast; the first one is not.** Once a project's server
is warm, a command typically answers in tens of milliseconds. The first
call against a project waits for the server to load it, which is seconds
for TypeScript and can be much longer for rust-analyzer on a large
workspace. `diagnostics` right after an edit waits (up to a few seconds)
for the server to finish re-checking. Bundled servers (Bash, HTML, CSS,
JSON) never wait. So `lsp` wins decisively on a large file or a
cross-file question, and is roughly even with `read` on a small one.

Edits you make between commands are picked up automatically, including
edits to files other than the one you query: no need to restart anything.

## Selecting a symbol: `--scope` and `--find`

`definition`, `reference`, `doc`, `symbol`, `calls`, `hierarchy`,
`rename` and `locate` take `--scope`. `outline`, `diagnostics` and
`search` do not: the first two describe a whole file, and `search` takes a
query instead.

| `--scope` | Means |
|---|---|
| `42` | Line 42 |
| `10,20` | Lines 10 to 20 |
| `10,0` | Line 10 to end of file |
| `MyClass` | The declaration of `MyClass`, through the end of its body |
| `MyClass.method` | `method` inside `MyClass` (any depth: `a.b.c`) |

One of `--scope` or `--find` is required: without either there is no
position, and the command fails rather than guessing. A line past the end
of the file is an error, not the last line.

A symbol scope covers the symbol's whole body and nothing after it, so
`--find` inside `--scope MyClass` only matches within `MyClass`, and
`Parent.child` only finds a `child` that belongs to `Parent`: declared in
its body, or for Rust in an `impl Parent` / `impl Trait for Parent` block,
for Go as a method with a `Parent` receiver, for Lua as `function
Parent.child`.

If a name is declared in more than one block (an `area` method in two
classes, say), the command fails and lists every candidate with its line:
qualify it (`Circle.area`) or use the line number. Several declarations in
the *same* block — overload signatures, a property's getter and setter —
are one symbol, and resolve to the first.

`--find <text>` narrows to an exact position inside that scope. It
ignores whitespace differences, and `<|>` marks where the cursor should
sit:

```bash
lsp definition src/service.ts --scope 12 --find "<|>User"
lsp doc src/models.ts --scope 22 --find "return <|>result"
```

When a command reports "Symbol not found" or resolves somewhere
unexpected, run `lsp locate` with the same arguments. It resolves the
position locally, with no language server involved, and prints the line
and column it picked with surrounding context:

```bash
lsp locate src/models.ts --scope User --find "greet"
```

Symbol-path scopes are matched with per-language declaration patterns, not
a parser. They handle the common declaration forms; if one does not
resolve, fall back to a line number from `outline`.

## Output

JSON by default, which is what to parse. `--output markdown` is for
showing a human. `--dry-run` prints the request that would be sent without
sending it.

Positions in JSON output: `line` is **1-based** (matching `--scope`),
`character` is a **0-based** offset in UTF-16 code units, as LSP itself
counts (the same as a character count unless the line has emoji or other
characters outside the Basic Multilingual Plane). Markdown output shows
both 1-based.

A failure exits 1 with the reason on stderr and nothing on stdout: a
position that doesn't resolve, a `symbol` scope with no symbol at it, a
`rename` the server can't perform. An empty result (no references, no
documentation) is not a failure.

`install` and `schema` take neither flag. `locate` and `server` take
`--output` but not `--dry-run`.

Every command has a short alias, which is worth using: `o`, `def`, `ref`,
`d`, `diag`, `c` (calls), `th` (hierarchy), `rn` (rename), `sym`, `l`
(locate), `s` (search), `i` (install), `srv` (server). `--project` is `-p`
everywhere it exists, which is everything except `locate`.

## Commands worth knowing in detail

Run `lsp <command> --help` for the full flag list, or `lsp schema
[command]` for a machine-readable JSON Schema. Only the parts that are
easy to get wrong are spelled out here.

### `rename`

The only command that writes files. Without `--apply` it previews and
touches nothing.

```bash
lsp rename src/models.ts --scope User.greet --new-name sayHello          # preview
lsp rename src/models.ts --scope User.greet --new-name sayHello --apply  # write
```

Always preview first and check the file list and edit count. Completeness
depends on the server having indexed the project, and on dynamically typed
languages being able to prove two usages are the same symbol at all. An
incomplete rename leaves the codebase half-renamed without announcing it,
which is worse than a wrong read-only answer. If the output reports
skipped file operations, the rename also needed to move or create a file
and is definitely incomplete. After `--apply`, search the old name to
confirm nothing was missed.

`--apply` is all or nothing: every file's edits are checked (in range,
non-overlapping) before any file is written, and if one doesn't apply,
nothing is written and the command fails. Files edited since the server
last saw them are re-read first, so the edits match what is on disk.

### What the results contain

- `calls` lists each caller (incoming) or callee (outgoing) with its own
  declaration (`uri`, `line`) and `callSites`: the lines where the calls
  are made. For incoming calls those are in the caller's file; for
  outgoing calls, in the file you queried. Edit at the call site, not
  the declaration.
- `symbol` returns the source with the `uri` and the `line`..`endLine` it
  came from, so you can edit it without looking it up again.
- `outline` is in source order, nested (members under their class).

### `reference` and `search` are paginated

Default page size is 20, configurable as `defaultMaxItems`.

```bash
lsp reference src/models.ts --scope User --max-items 20 --start-index 20
```

Both report `total` and `startIndex` in their JSON, and `reference` also
`truncated` (true when more results follow this page), so you can tell
whether to ask for the next page. With an LSP backend, `search`'s `total`
is whatever the server returned, and servers cap it (rust-analyzer at 128),
so narrow the query rather than paging deep.

`--pagination-id` is accepted but does nothing; each call re-queries.

### `search`

```bash
lsp search "User" --kinds class --kinds interface
```

`--kinds` is repeatable. Valid values: `class`, `interface`, `function`,
`method`, `variable`, `constant`, `enum`, `struct`, and the other LSP
symbol kinds. An invalid one is an error, not an empty result.

`search` covers the project around the current directory (the nearest
directory above it with a root marker such as `package.json`, `Cargo.toml`,
`go.mod`, or `.git`), or `--project <path>`. Outside any project it refuses
to run from your home directory, `/` or the temp directory.

It asks a language server already running for that project (any earlier
`outline`, `definition`, ... warms one), and otherwise answers from a
built-in index without starting one. The JSON says which: `"backend":
"lsp"` or `"bm25"`. The built-in index finds fewer things and ranks them
less precisely; if you need the server's answer, run any navigation
command on a file in the project first.

### `diagnostics`

```bash
lsp diagnostics src/service.ts
```

Run after editing a file to check it still typechecks, instead of invoking
the project's build tool. Not every server supports it; the error says so
explicitly when the request itself failed. An empty list means no problems
found, which is not the same as unsupported.

### `install` and `server`

Language servers install themselves on first use. You should not normally
need these.

```bash
lsp install --list          # what's installed
lsp install typescript      # one language
lsp install --all           # everything
lsp install rust --update   # reinstall

lsp server list             # what's running, with pid and idle time
lsp server stop <project>   # force a respawn on the next call
lsp server shutdown         # stop the daemon itself
```

Java needs a JDK already present. Deno is used if it is on `PATH` but is
never installed for you.

## How it behaves

A language server starts on first use and stays warm in a background
daemon, shared across CLI invocations, until it has been idle for ten
minutes. While it is warm, edits made anywhere in the project are picked
up automatically, so you do not need to restart anything after editing.

Configuration lives in `~/.lsp-cli/config.json`, all durations in
**seconds**:

```json
{ "idleTimeout": 600, "managerTimeout": 60, "defaultMaxItems": 20, "usePathServers": true }
```

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| "`X` is declared N times in scope" | The error lists every declaration with its line. Qualify the name (`Parent.X`) or pass the line number. |
| "Symbol not found", or a result from the wrong place | Run `lsp locate` with the same `--scope`/`--find` to see what position it resolved to. If a symbol path does not resolve, use a line number from `outline`. |
| "Cannot detect project root" | The file is not under a recognized root marker. Pass `--project <path>`. |
| "Unsupported file type" | The extension is not one of the supported languages. Use grep. |
| `invalid value '...' for '--mode'` / `--direction` / `--output` | Rejected at parse time; the error lists the valid values. `calls` uses `incoming`/`outgoing`, `hierarchy` uses `subtypes`/`supertypes`. |
| `Unknown --kinds value(s)` | Same idea for `search --kinds`; the message lists every valid kind. |
| "The language server exited. Its last output: ..." | The server crashed; what it printed is in the error (and in `~/.lsp-cli/logs/daemon.log`). The next command starts it again. |
| "restarting the background daemon: it is from a different build" | Expected once after upgrading or rebuilding `lsp`. |
| A command hangs, or results look stale | `lsp server list` to see what is running, then `lsp server stop <project>` to force a respawn. `lsp server shutdown` if the daemon itself is wedged. |
| "does not support `textDocument/prepareTypeHierarchy`" | typescript-language-server, basedpyright and rust-analyzer don't implement type hierarchy. Use `lsp reference --scope X --mode implementations`. Any "does not support" error is the server's limit, not this tool's. |
| `outline --scope` is rejected | `outline` describes a whole file. Use `lsp symbol` for one symbol. |

## MCP

`lsp mcp` runs the CLI as an MCP server over stdio, exposing the same
commands as tools. Use this instead of shell invocations if the host
supports it.
