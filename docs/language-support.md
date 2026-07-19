# Language support

Per-language server and test-coverage status. See
[../CONTRIBUTING.md](../CONTRIBUTING.md) for how to install these servers
locally to run the gated tests referenced below.

| Language | Server | Status |
|---|---|---|
| TypeScript | `typescript-language-server` + local `typescript` | ✅ outline/definition/reference/doc/symbol/search, `tests/{outline,definition,reference,doc,symbol,search,calls,diagnostics}.rs` |
| Python | `basedpyright-langserver` (npm) | ✅ outline/definition/reference/doc, `tests/python.rs` |
| Go | `gopls` (`go install golang.org/x/tools/gopls@latest`) | ✅ outline/definition/doc, `tests/go_lang.rs` |
| Rust | `rust-analyzer` (`rustup component add rust-analyzer`) | ✅ outline/definition/doc, `tests/rust_lang.rs` |
| CSS | `vscode-css-language-server` (npm `vscode-langservers-extracted`) | ✅ outline/doc, `tests/web.rs` |
| JSON | `vscode-json-language-server` (same package) | ✅ outline (with `--all`; JSON keys aren't in the default top-level symbol-kind filter), `tests/web.rs` |
| HTML | `vscode-html-language-server` (same package) | ⚠️ spawns and responds correctly, but returns *flat* `SymbolInformation[]` for `textDocument/documentSymbol` instead of hierarchical `DocumentSymbol[]`. Outline only ever deserializes the hierarchical shape, so it comes back empty. `tests/web.rs` asserts the (empty-but-valid) shape. |
| Java | `jdtls` (auto-installable: fetches Eclipse's release tarball, requires a JDK already present via sdkman/`$JAVA_HOME`/PATH) | ✅ outline verified against `tests/fixtures/java_project/`. `tests/java_kotlin.rs`. |
| Kotlin | `kotlin-language-server` (auto-installable: fetches the latest GitHub Release zip) | ✅ installed and spawns for real. `tests/java_kotlin.rs`. |
| C / C++ | `clangd` (auto-installable: fetches the latest GitHub Release zip, extracts the version-stamped top-level dir to a fixed `clangd/` path) | ✅ verified live: real `outline` against a two-function C file returned both functions with correct signatures (`int (int, int)` for `add`, `int ()` for `main`). No dedicated `tests/*.rs` file yet. |
| Lua | `lua-language-server` (auto-installable: fetches the latest GitHub Release tar.gz) | ✅ verified live: real `outline` against a file with a local function returned it with its parameter as a nested symbol. No dedicated `tests/*.rs` file yet. |
| Zig | `zls` (auto-installable: fetches the latest GitHub Release tar.xz, a bare binary at the archive root with no wrapping directory) | ✅ verified live: real `outline` against a `pub fn main` returned it with the correct `fn main() void` signature. No dedicated `tests/*.rs` file yet. |
| Bash / shell | `bash-language-server` (npm, auto-installable) | ⚠️ `doc`(hover)/`reference`/`definition` verified live and work correctly (hover on `echo` returned the real bash builtin man-page text; reference correctly found a function call site), but `outline` (`textDocument/documentSymbol`) returns an empty list even for a file with a declared function, confirmed live. This appears to be a genuine limitation of this server's document-symbol support rather than anything on this tool's side (its declared LSP capabilities are otherwise complete). No dedicated `tests/*.rs` file yet. |
| C# | `csharp-ls` (via `dotnet tool install --tool-path`) | ✅ verified live once a .NET SDK was made available: `outline` against a two-class file returned the correct `CsTest` namespace to `Greeter` (field/constructor/method) and `Program` (Main) tree; `definition` on a constructor call resolved correctly; `doc` (hover) returned the correct method signature. No dedicated `tests/*.rs` file yet. |
| Ruby | `ruby-lsp` (via `gem install --bindir`) | ✅ verified live end-to-end through the real `lsp` CLI/daemon (`outline` and `definition` against `tests/fixtures/ruby_project/greeter.rb` both returned correct results; `doc`/hover returned "no documentation available" for a plain method with no RBS/comment doc, which appears to be expected server behavior rather than a bug). Getting here required fixing the host's Ruby environment, not this tool's code. See [CONTRIBUTING.md#ruby-ruby-lsp](../CONTRIBUTING.md#ruby-ruby-lsp) for the exact steps: `ruby-lsp` always composes a Bundler-managed bundle on startup, which needs (1) Bundler installed via `gem install --user-install` (not the read-only system gem dir), (2) `PATH` including that user gem bin dir so `ruby-lsp` can find `bundle`, (3) `BUNDLE_PATH` pointed at a writable location so the composed bundle doesn't try to install into the system gem cache, and (4) the `libyaml-dev` system package so the `psych` gem's native extension can compile. None of that is something `lsp-cli-rust` can configure on the user's behalf; it's host environment setup. No dedicated `tests/*.rs` file yet (needs the above host setup to pass in CI). |
| Deno | `deno lsp` (detected on `PATH`, never auto-installed) | ✅ outline/definition/reference/doc all verified against a real `deno` binary. No dedicated fixture/test file yet. |

A real bug was found and fixed while adding C/C++: `install_clangd`'s final step moved (`rename`'d) the extracted directory from the system temp dir into `~/.lsp-cli/servers/`. On a machine where `/tmp` is a different filesystem/mount than the home directory, `rename` across filesystems fails with `EXDEV`, reproduced live. Fixed by extracting directly under `~/.lsp-cli/servers/` (a staging subdirectory on the same filesystem as the final destination) instead of the system temp dir.

The language → server-binary/extension/root-marker mapping lives in
`src/registry.rs`. See [architecture.md](architecture.md) for how it's
used. Adding a new language means one `LanguageConfig` entry there plus,
if it should be auto-installable, a case in `src/install.rs`.

## TypeScript vs. Deno

"TypeScript" and "Deno" are not two different programming languages. The
registry has two entries for the same file extensions
(`.ts`/`.tsx`/`.js`/`.jsx`) because they're two different runtimes/
toolchains for that one language, and each needs a different language
server:

| Registry entry | Language server | Module resolution |
|---|---|---|
| `typescript` | `typescript-language-server` (wraps `tsserver`) | Node-style: `node_modules`, npm packages |
| `deno` | `deno lsp` (built into the Deno runtime) | Deno-style: URL imports, JSR specifiers, no `node_modules` |

These diverge enough (resolution, type-checking behavior, built-in
formatting/linting on Deno's side) that one server can't correctly serve
both: a Deno-style `https://deno.land/...` or `jsr:` import means nothing
to `tsserver`, and `tsserver`'s `node_modules` resolution means nothing to
`deno lsp`.

**Which one a project gets** is decided by `detect_project_root` in
`src/registry.rs`, walking up from the target file looking for a root
marker:

- `deno.json` or `deno.jsonc` present → `deno`
- otherwise, `package.json`, `tsconfig.json`, or `jsconfig.json` present →
  `typescript`

`deno` is checked first in registry order, so a project with **both**
markers (a Deno project that also keeps a `package.json` for npm interop,
which is fairly common) resolves to `deno`. The reverse (a Node project
with a stray `deno.json`) is rare enough not to matter in practice, but if
it happens the same rule applies: `deno.json` wins.
