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
pub fn language_id(language: &str) -> &str {
    match language {
        "deno" => "typescript",
        other => other,
    }
}
