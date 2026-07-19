//! Language server registry: maps file extensions / project root markers to
//! LSP server launch commands. Mirrors languages/registry.ts.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub struct LanguageConfig {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub root_markers: &'static [&'static str],
    pub server_bin: &'static str,
    pub server_args: fn(&str) -> Vec<String>,
}

/// Every field is `'static` data (string/slice literals, function
/// pointers), so this is a plain static array instead of a `Vec` built
/// fresh on every call — `languages()` used to heap-allocate a new `Vec`
/// on every single call, including from inside the per-directory-level
/// loop in `detect_project_root` (called once per ancestor directory
/// walked) and per-file in `detect_language`.
pub fn languages() -> &'static [LanguageConfig] {
    &[
        // `deno` and `typescript` below share the same file extensions —
        // they're not different languages, just two different runtimes/
        // toolchains for TS/JS, each needing its own LSP (Deno's built-in
        // `deno lsp` vs. `typescript-language-server` wrapping `tsserver`).
        // They diverge enough in module resolution (URL/JSR imports vs.
        // node_modules) that one server can't serve both correctly.
        // `detect_project_root` walks root markers in this array's order,
        // so `deno` is checked first: a project with both `deno.json` and
        // `package.json` (common for a Deno project with npm interop)
        // resolves to `deno`. See docs/language-support.md#typescript-vs-deno.
        LanguageConfig {
            name: "deno",
            extensions: &[".ts", ".tsx", ".js", ".jsx"],
            root_markers: &["deno.json", "deno.jsonc"],
            server_bin: "deno",
            server_args: |_| vec!["lsp".to_string()],
        },
        LanguageConfig {
            name: "typescript",
            extensions: &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
            root_markers: &["package.json", "tsconfig.json", "jsconfig.json"],
            server_bin: "typescript-language-server",
            server_args: |_| vec!["--stdio".to_string()],
        },
        LanguageConfig {
            name: "python",
            extensions: &[".py", ".pyi"],
            root_markers: &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", ".python-version"],
            server_bin: "basedpyright-langserver",
            server_args: |_| vec!["--stdio".to_string()],
        },
        LanguageConfig {
            name: "go",
            extensions: &[".go"],
            root_markers: &["go.mod", "go.work"],
            server_bin: "gopls",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "rust",
            extensions: &[".rs"],
            root_markers: &["Cargo.toml", "Cargo.lock"],
            server_bin: "rust-analyzer",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "java",
            extensions: &[".java"],
            root_markers: &["pom.xml", "build.gradle", "build.gradle.kts", ".classpath"],
            server_bin: "jdtls",
            server_args: |root| vec!["-data".to_string(), root.to_string()],
        },
        LanguageConfig {
            name: "kotlin",
            extensions: &[".kt", ".kts"],
            root_markers: &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "pom.xml"],
            server_bin: "kotlin/server/bin/kotlin-language-server",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "cpp",
            extensions: &[".c", ".h", ".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx"],
            root_markers: &["compile_commands.json", "CMakeLists.txt", ".clangd", "Makefile"],
            server_bin: "clangd/bin/clangd",
            server_args: |_| vec!["--background-index".to_string()],
        },
        LanguageConfig {
            name: "lua",
            extensions: &[".lua"],
            root_markers: &[".luarc.json", ".luarc.jsonc", "selene.toml", "stylua.toml"],
            server_bin: "lua/bin/lua-language-server",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "zig",
            extensions: &[".zig"],
            root_markers: &["build.zig"],
            server_bin: "zls",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "ruby",
            extensions: &[".rb"],
            root_markers: &["Gemfile", "Rakefile", ".ruby-version"],
            server_bin: "ruby-lsp",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "csharp",
            extensions: &[".cs"],
            // No fixed-filename root marker exists for C# (project/solution
            // files are `*.csproj`/`*.sln` with a variable stem, and
            // `root_markers` here only matches literal filenames, not
            // globs) — falls back to the file's own directory, same as
            // html/css/json below.
            root_markers: &[],
            server_bin: "csharp-ls",
            server_args: |_| vec![],
        },
        LanguageConfig {
            name: "bash",
            extensions: &[".sh", ".bash"],
            root_markers: &[],
            server_bin: "bash-language-server",
            server_args: |_| vec!["start".to_string()],
        },
        LanguageConfig {
            name: "html",
            extensions: &[".html", ".htm"],
            root_markers: &[],
            server_bin: "vscode-html-language-server",
            server_args: |_| vec!["--stdio".to_string()],
        },
        LanguageConfig {
            name: "css",
            extensions: &[".css", ".scss", ".less"],
            root_markers: &[],
            server_bin: "vscode-css-language-server",
            server_args: |_| vec!["--stdio".to_string()],
        },
        LanguageConfig {
            name: "json",
            extensions: &[".json", ".jsonc"],
            root_markers: &[],
            server_bin: "vscode-json-language-server",
            server_args: |_| vec!["--stdio".to_string()],
        },
    ]
}


pub fn server_path(installed_bin_name: &str, install_dir: &Path) -> PathBuf {
    // deno relies on PATH; everything else is installed under install_dir
    if installed_bin_name == "deno" {
        PathBuf::from("deno")
    } else {
        install_dir.join(installed_bin_name)
    }
}

pub fn detect_language(file_path: &Path) -> Option<LanguageConfig> {
    let ext = format!(".{}", file_path.extension()?.to_str()?.to_lowercase());
    languages().iter().copied().find(|l| l.name != "deno" && l.extensions.contains(&ext.as_str()))
}

pub struct Detected {
    pub lang: LanguageConfig,
    pub root: PathBuf,
}

/// Walk up from file_path looking for a root marker matching a known language.
/// Falls back to the file's own directory for languages with no root markers
/// (html, css, json, csharp, bash).
pub fn detect_project_root(file_path: &Path) -> Option<Detected> {
    let ext = format!(
        ".{}",
        file_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
    );

    let candidates: Vec<LanguageConfig> = languages().iter().copied().filter(|l| l.extensions.contains(&ext.as_str())).collect();
    if candidates.is_empty() {
        return None;
    }

    // `file_path` may point at a file that doesn't exist yet (e.g. the
    // synthetic `<dir>/index.ts` probe used by `daemon.rs::create` to accept
    // a bare directory), so `canonicalize()` can fail here even though the
    // containing directory is real. Falling back to the raw, possibly
    // relative/non-canonical path in that case used to make the returned
    // `root` non-canonical too — which, since navigation commands always
    // canonicalize via `project::resolve_project`, made the same project
    // resolve to two different `project_root` keys depending on which code
    // path detected it, spawning a duplicate warm server instead of reusing
    // the pre-warmed one. Canonicalize the *parent directory* directly
    // (which exists even when the file itself doesn't) so `root` is always
    // canonical regardless of which caller triggered detection.
    let start_dir = file_path
        .parent()?
        .canonicalize()
        .or_else(|_| file_path.canonicalize().and_then(|p| p.parent().map(|p| p.to_path_buf()).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))))
        .unwrap_or_else(|_| file_path.parent().unwrap_or(file_path).to_path_buf());
    let mut dir = start_dir.clone();
    loop {
        for lang in &candidates {
            for marker in lang.root_markers {
                if dir.join(marker).exists() {
                    return Some(Detected { lang: *lang, root: dir });
                }
            }
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }

    for lang in &candidates {
        if lang.root_markers.is_empty() {
            return Some(Detected { lang: *lang, root: start_dir });
        }
    }

    None
}

pub fn default_install_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".lsp-cli").join("servers")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_typescript_by_extension() {
        let lang = detect_language(Path::new("foo/bar.ts")).unwrap();
        assert_eq!(lang.name, "typescript");
    }

    #[test]
    fn detects_rust_project_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let detected = detect_project_root(&file).unwrap();
        assert_eq!(detected.lang.name, "rust");
        assert_eq!(detected.root, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn falls_back_for_json_with_no_root_marker() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.json");
        std::fs::write(&file, "{}\n").unwrap();
        let detected = detect_project_root(&file).unwrap();
        assert_eq!(detected.lang.name, "json");
    }
}
