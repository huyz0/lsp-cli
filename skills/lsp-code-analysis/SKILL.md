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
| What extends X? | `lsp hierarchy <file> --scope X` |
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

**Each command costs a few seconds.** Warm calls carry a fixed ~3s wait
for the server to process the file; the first call against a project also
waits for its initial index, which is seconds for TypeScript and can be
much longer for rust-analyzer on a large workspace. Bundled servers
(Bash, HTML, CSS, JSON) have no such wait. So `lsp` wins decisively on a
large file or a cross-file question, and loses to `read` on a small one.
Do not sweep twenty symbols one at a time.

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
| `MyClass` | The declaration of `MyClass` |
| `MyClass.method` | `method` inside `MyClass` |

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

### `reference` and `search` are paginated

Default page size is 20, configurable as `defaultMaxItems`.

```bash
lsp reference src/models.ts --scope User --max-items 20 --start-index 20
```

`search`'s JSON reports `total` and `startIndex`, so you can tell whether
more results exist. **`reference`'s JSON does not**: its "N more
results" notice goes to stderr. If you are capturing stdout only, compare the
number of locations against `--max-items` to detect truncation.

`--pagination-id` is accepted but does nothing; each call re-queries.

### `search`

```bash
lsp search "User" --kinds class --kinds interface
```

`--kinds` is repeatable. Valid values: `class`, `interface`, `function`,
`method`, `variable`, `constant`, `enum`, `struct`, and the other LSP
symbol kinds. An invalid one is an error, not an empty result.

`search` needs a project to search. Run it from inside the project, or
pass `--project <path>`. It uses the language server's workspace symbol
index when one answers, and falls back to a built-in text index otherwise.
The fallback finds fewer things and ranks them less precisely, so prefer
running from a real project root.

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
{ "idleTimeout": 600, "managerTimeout": 60, "defaultMaxItems": 20 }
```

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| "Symbol not found", or a result from the wrong place | Run `lsp locate` with the same `--scope`/`--find` to see what position it resolved to. If a symbol path does not resolve, use a line number from `outline`. |
| "Cannot detect project root" | The file is not under a recognized root marker. Pass `--project <path>`. |
| "Unsupported file type" | The extension is not one of the supported languages. Use grep. |
| `invalid value '...' for '--mode'` / `--direction` / `--output` | Rejected at parse time; the error lists the valid values. `calls` uses `incoming`/`outgoing`, `hierarchy` uses `subtypes`/`supertypes`. |
| `Unknown --kinds value(s)` | Same idea for `search --kinds`; the message lists every valid kind. |
| A command hangs, or results look stale | `lsp server list` to see what is running, then `lsp server stop <project>` to force a respawn. `lsp server shutdown` if the daemon itself is wedged. |
| `hierarchy` fails on TypeScript | `typescript-language-server` does not implement type hierarchy. Not a bug in this tool. |
| `outline --scope` is rejected | `outline` describes a whole file. Use `lsp symbol` for one symbol. |

## MCP

`lsp mcp` runs the CLI as an MCP server over stdio, exposing the same
commands as tools. Use this instead of shell invocations if the host
supports it.
