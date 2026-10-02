use crate::registry::detect_project_root;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

pub struct ProjectContext {
    pub file_path: PathBuf,
    pub project_root: PathBuf,
    pub language: String,
    pub uri: String,
}

pub fn resolve_project(file_path: &str, project_override: Option<&str>) -> Result<ProjectContext> {
    let abs_file = Path::new(file_path)
        .canonicalize()
        .map_err(|_| anyhow!("File not found: {file_path}"))?;

    // Root detection and the `--project` override are resolved
    // independently, because detection is allowed to fail when an override
    // is present. It used to be `detect_project_root(...)?` first, which
    // made the override unreachable for exactly the case its own error
    // message advertises it for ("Or use --project <path>"): a file with no
    // recognized root marker anywhere above it. The language still has to
    // come from somewhere, so fall back to extension-based detection, which
    // needs no markers.
    let detected = detect_project_root(&abs_file);

    let override_root = match project_override {
        Some(p) => Some(
            PathBuf::from(p)
                .canonicalize()
                .map_err(|e| anyhow!("--project path not found: {p} ({e})"))?,
        ),
        None => None,
    };

    let language = match &detected {
        Some(d) => d.lang.name.to_string(),
        None => crate::registry::detect_language(&abs_file)
            .ok_or_else(|| {
                anyhow!(
                    "Unsupported file type: {}\nHint: `lsp install --list` shows every language this tool recognizes.",
                    abs_file.display()
                )
            })?
            .name
            .to_string(),
    };

    let project_root = match (override_root, detected) {
        (Some(root), _) => root,
        (None, Some(d)) => d.root,
        (None, None) => {
            return Err(anyhow!(
                "Cannot detect project root for: {}\nHint: ensure the file is inside a project with a recognized root marker \
                 (package.json, go.mod, pyproject.toml, Cargo.toml, etc.)\nOr use --project <path> to specify the root explicitly.",
                abs_file.display()
            ))
        }
    };

    Ok(ProjectContext {
        uri: lsp::uri::from_path(&abs_file),
        file_path: abs_file,
        project_root,
        language,
    })
}

/// LSP `languageId` for a `textDocument/didOpen` notification.
///
/// Keyed on the file's extension, not just the registry language: the
/// identifiers the spec lists distinguish `typescriptreact` from
/// `typescript`, `javascript` from both, `c` from `cpp`, and call shell
/// scripts `shellscript`. Sending the registry name ("typescript") for a
/// `.tsx` or `.js` file told typescript-language-server to parse JSX as
/// plain TypeScript.
pub fn language_id(language: &str, path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match (language, ext.as_str()) {
        ("typescript" | "deno", "tsx") => "typescriptreact",
        ("typescript" | "deno", "jsx") => "javascriptreact",
        ("typescript" | "deno", "js" | "mjs" | "cjs") => "javascript",
        ("typescript" | "deno", _) => "typescript",
        ("cpp", "c") => "c",
        ("cpp", _) => "cpp",
        ("bash", _) => "shellscript",
        ("css", "scss") => "scss",
        ("css", "less") => "less",
        ("css", _) => "css",
        ("json", "jsonc") => "jsonc",
        ("json", _) => "json",
        ("python", _) => "python",
        ("go", _) => "go",
        ("rust", _) => "rust",
        ("java", _) => "java",
        ("kotlin", _) => "kotlin",
        ("lua", _) => "lua",
        ("zig", _) => "zig",
        ("ruby", _) => "ruby",
        ("csharp", _) => "csharp",
        ("html", _) => "html",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_follow_the_extension() {
        let id = |lang, file| language_id(lang, Path::new(file));
        assert_eq!(id("typescript", "a/b.ts"), "typescript");
        assert_eq!(id("typescript", "a/b.mts"), "typescript");
        assert_eq!(id("typescript", "a/B.TSX"), "typescriptreact");
        assert_eq!(id("typescript", "a/b.jsx"), "javascriptreact");
        assert_eq!(id("typescript", "a/b.js"), "javascript");
        assert_eq!(id("typescript", "a/b.cjs"), "javascript");
        assert_eq!(id("deno", "mod.tsx"), "typescriptreact");
        assert_eq!(id("deno", "mod.ts"), "typescript");
        assert_eq!(id("cpp", "x.c"), "c");
        assert_eq!(id("cpp", "x.h"), "cpp");
        assert_eq!(id("cpp", "x.cc"), "cpp");
        assert_eq!(id("bash", "run.sh"), "shellscript");
        assert_eq!(id("css", "s.scss"), "scss");
        assert_eq!(id("json", "tsconfig.jsonc"), "jsonc");
        assert_eq!(id("csharp", "A.cs"), "csharp");
    }

    #[test]
    fn every_registry_language_has_an_explicit_id() {
        for lang in crate::registry::languages() {
            let ext = lang.extensions[0].trim_start_matches('.');
            let id = language_id(lang.name, Path::new(&format!("f.{ext}")));
            assert_ne!(id, "plaintext", "{} has no languageId", lang.name);
        }
    }
}
