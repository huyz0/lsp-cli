---
name: lsp-code-analysis
description: Code intelligence via LSP: definitions, references, outlines, docs, call hierarchy, diagnostics. Default to this over grep/read for code understanding tasks. It returns just the matching symbol or location instead of a full file or a page of grep noise to filter by hand, and does what grep/read can't: verify a file compiles, trace real call sites instead of text matches.
---

# LSP Code Analysis

## Why this over grep/read

The cost difference is the point, not a side benefit. `read <file>` charges for
the whole file even when you want one 15-line function; `grep -r "X"` charges
for every match including imports, comments, and unrelated variables sharing
the name X, and text-pattern guessing then usually forces a second `read` to
disambiguate what actually matched. `lsp symbol`/`lsp definition`/`lsp
reference` skip both costs: they return the one symbol or the exact
structurally-correct locations, nothing else to read through or filter.

Default to `lsp` for definitions, references, outlines, docs, and diagnostics.
Reach for `read`/`grep` only for literal string searches, comments, or content
that isn't code at all (README prose, config values, log output).

## Prerequisites

The `lsp` binary must be installed. Check with:

```bash
lsp --version
```

If missing, install it:

```bash
# Homebrew (macOS/Linux)
brew install huyz0/tap/lsp-cli

# no package manager
curl -fsSL https://raw.githubusercontent.com/huyz0/lsp-cli/main/install.sh | sh

# or download a prebuilt binary directly from
# https://github.com/huyz0/lsp-cli/releases
```

Or build it from source:

```bash
cargo install --path <path-to-lsp-cli>
# or, from inside the repo:
cargo build --release   # binary at ./target/release/lsp
```

Language servers are managed separately. Install the server for your language:

```bash
lsp install --list              # see what's installed
lsp install typescript          # install TypeScript/JS server
lsp install python              # install Python (basedpyright)
lsp install go                  # install Go (gopls)
lsp install java                # install Java (jdtls), requires a JDK already
                                 # present (sdkman, $JAVA_HOME, or `java` on PATH)
```

**Auto-start**: A language server starts automatically on the first navigation
command and stays warm in a background daemon, reused across calls (even
across separate CLI invocations) until it's been idle for 10 minutes. You do
not need to call `lsp server start` manually.

**deno** is detected on `PATH` and used if present, but never auto-installed.
Install it yourself from https://deno.land if needed.

## Commands

All commands output JSON by default (optimized for agents). Use `--output
markdown` for human-readable output, and `--dry-run` to preview the LSP
request without sending it. Every flag has a description in `--help`.

### `lsp outline <file>`

Get a file's symbol structure without reading its full implementation.

```bash
lsp outline src/models.ts
lsp outline src/models.ts --all   # include fields and parameters
```

**Use this first** when exploring an unfamiliar file.

### `lsp definition <file> --scope <symbol>`

Navigate to where a symbol is defined.

```bash
lsp definition src/service.ts --scope createUser
lsp definition src/service.ts --scope 12 --find "<|>User"
lsp definition src/service.ts --scope createUser --mode type_definition
```

Modes: `definition` (default), `declaration`, `type_definition`

### `lsp reference <file> --scope <symbol>`

Find all usages of a symbol across the workspace.

```bash
lsp reference src/models.ts --scope User
lsp reference src/models.ts --scope User --mode implementations
lsp reference src/models.ts --scope User --max-items 20 --start-index 0 --pagination-id ref1
```

### `lsp doc <file> --scope <symbol>`

View type signature and documentation for a symbol.

```bash
lsp doc src/models.ts --scope User.greet
lsp doc src/models.ts --scope 22 --find "<|>greet"
```

### `lsp calls <file> --scope <symbol>`

Find who calls, or is called by, a symbol.

```bash
lsp calls src/service.ts --scope createUser                       # who calls createUser (default: incoming)
lsp calls src/service.ts --scope createUser --direction outgoing  # what createUser calls
```

More precise than `reference` for impact analysis. It only follows actual
call sites, not every textual usage (imports, type annotations, reads/writes
of a variable with the same name).

### `lsp diagnostics <file>`

Report compiler/type-checker errors and warnings for a file.

```bash
lsp diagnostics src/service.ts
```

**Run this after editing a file** to check it still compiles/typechecks,
instead of invoking the project's own build tool. Not every language server
supports this yet. If it fails, the error message says so explicitly.

### `lsp symbol <file> --scope <symbol>`

Get the full source code of a symbol.

```bash
lsp symbol src/models.ts --scope User.greet
lsp symbol src/models.ts --scope 22
```

Prefer this over `read` for targeted code inspection: avoids loading entire files.

### `lsp search "<query>"`

Find symbols by name across the workspace.

```bash
lsp search "User"
lsp search "create" --kinds function
lsp search "User" --max-items 20 --start-index 0 --pagination-id s1
```

Kind values: `class`, `interface`, `function`, `method`, `variable`, `constant`, `enum`, `struct`

### `lsp locate <file> --scope <scope>`

Verify that a scope+find pattern resolves correctly before using it in other commands. Runs purely locally: no LSP server, no daemon, no network. Use this whenever `--find` isn't matching what you expect, instead of debugging by trial and error against the LSP commands.

```bash
lsp locate src/models.ts --scope User
lsp locate src/models.ts --scope 15 --find "return <|>result"
```

### `lsp server list`

Show running servers, their PID, and idle time.

```bash
lsp server list
```

## Schema & Introspection

Agents can dynamically list commands and retrieve the exact JSON Schema for their input arguments using the `schema` command instead of relying on this documentation:

```bash
lsp schema              # list all endpoint schemas
lsp schema definition   # show the input schema for `definition`
```

## Locate Syntax

The `--scope` and `--find` options are shared across all navigation commands (`outline`, `definition`, `reference`, `doc`, `symbol`, `locate`):

| Format | Meaning |
|--------|---------|
| `42` | Line 42 |
| `10,20` | Lines 10-20 |
| `MyClass` | Top-level symbol named `MyClass` |
| `MyClass.method` | Nested symbol |
| `10,0` | Line 10 to end of file |

`--find <pattern>`: Searches for a text pattern within the scope.
- Whitespace-insensitive (extra spaces ignored)
- `<|>` marks the exact cursor position within the pattern
- If omitted, the position defaults to the start of `--scope`

If a command errors with "Symbol not found" or returns unexpected results,
run `lsp locate` with the same `--scope`/`--find` first. It's free (no LSP
round trip) and will show exactly what position it resolved to.

## Pagination

For `reference` and `search`:

```bash
# Page 1
lsp reference src/models.ts --scope User --max-items 20 --pagination-id task123

# Page 2
lsp reference src/models.ts --scope User --max-items 20 --start-index 20 --pagination-id task123
```

`--pagination-id` is accepted for interface compatibility but each call still
queries the LSP server fresh. There's no server-side cursor to invalidate,
so re-running page 1 after other edits is safe.

## Recommended Workflows

### Understanding an unfamiliar file

```bash
# 1. Get structure without reading implementation
lsp outline src/models.ts

# 2. Inspect signatures of interesting symbols
lsp doc src/models.ts --scope User.greet

# 3. Navigate to dependencies
lsp definition src/models.ts --scope User

# 4. Find where it's used
lsp reference src/models.ts --scope User
```

### Change impact analysis

```bash
# Find all callers of a function
lsp reference src/service.ts --scope createUser

# Trace data flow: find where the type is used
lsp search "User" --kinds class
lsp reference src/models.ts --scope User

# Or, for call sites specifically (excludes non-call usages)
lsp calls src/service.ts --scope createUser
```

### Debugging

```bash
# Find symbol workspace-wide
lsp search "processData"

# Verify the definition
lsp definition src/service.ts --scope processData

# Trace all callers
lsp reference src/service.ts --scope processData
```

## Tool Selection Guide

| Task | Traditional | Recommended |
|------|-------------|-------------|
| Understand a file | `read <file>` | `lsp outline <file>` |
| Find where X is defined | `grep -r "X"` | `lsp definition <file> --scope X` |
| Find usages of X | `grep -r "X"` | `lsp reference <file> --scope X` |
| Find callers of X | `grep -r "X("` | `lsp calls <file> --scope X` |
| View docs/types | `read <file>` | `lsp doc <file> --scope X` |
| Read a function | `read <file>` | `lsp symbol <file> --scope X` |
| Find a class | `grep -r "class X"` | `lsp search "X" --kinds class` |

**Rule**: Prefer `lsp` commands over `read`/`grep` for all code understanding tasks. Use `read`/`grep` only for literal string searches or comments.

## Behavior notes

- **Warm servers**: navigation commands proxy through a background daemon and
  reuse a live language server for the same project across calls (default
  10-minute idle timeout, configurable via `~/.lsp-cli/config.json`'s
  `idleTimeout`, in milliseconds). The first call for a project pays LSP
  startup cost; subsequent calls are fast.
- **File watching**: while a server is warm, external edits to files in its
  project are detected automatically and pushed to the server, so you don't
  need to restart the server after editing a file outside of the command
  you're running.
- If a server seems stuck or stale after an external process crashed it,
  `lsp server stop <project>` (or `--all`) forces a clean respawn on the next
  call.

## Troubleshooting

| Symptom | Try this |
|---|---|
| "Symbol not found" or wrong location resolved | Run `lsp locate <file> --scope ... --find ...` with the same arguments. It's free (no LSP round trip) and shows exactly what position it resolved to. |
| "Cannot detect project root" | The file isn't under a recognized root marker (package.json, Cargo.toml, go.mod, etc). Pass `--project <path>` explicitly. |
| Command errors with an "Unknown mode"/"Unknown --output value" message | The value you passed is invalid and was rejected. These commands fail loudly rather than silently falling back to a default, so check the message's list of valid values. |
| Results look stale after you (or another process) edited a file | Should self-correct: warm servers watch their project directory and get notified of external edits automatically. If it doesn't, `lsp server stop <project>` forces a clean respawn. |
| A navigation command hangs or a server seems wedged | `lsp server list` to see what's running and its PID, then `lsp server stop <project>` (or `--all`) to force a respawn on the next call. |
| `lsp diagnostics` returns no items but you know there's an error | Not every language server supports this yet (the error message says so explicitly if the request itself failed). If it returned an *empty* list instead of erroring, the server may not have finished analyzing yet. This is best-effort and doesn't retry. |
| Auto-install fails for `java` | Requires a JDK already present via sdkman, `$JAVA_HOME`, or `java` on `PATH`. This tool won't install a JDK for you. |
| `deno` isn't detected | It's never auto-installed, must already be on `PATH`. Install it from https://deno.land. |
