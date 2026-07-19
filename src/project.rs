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

    let detected = detect_project_root(&abs_file).ok_or_else(|| {
        anyhow!(
            "Cannot detect project root for: {}\nHint: ensure the file is inside a project with a recognized root marker \
             (package.json, go.mod, pyproject.toml, Cargo.toml, etc.)\nOr use --project <path> to specify the root explicitly.",
            abs_file.display()
        )
    })?;

    let project_root = match project_override {
        Some(p) => PathBuf::from(p)
            .canonicalize()
            .map_err(|e| anyhow!("--project path not found: {p} ({e})"))?,
        None => detected.root,
    };

    Ok(ProjectContext {
        uri: format!("file://{}", abs_file.display()),
        file_path: abs_file,
        project_root,
        language: detected.lang.name.to_string(),
    })
}

/// LSP `languageId` for a `textDocument/didOpen` notification.
pub fn language_id(language: &str) -> &str {
    match language {
        "deno" => "typescript",
        other => other,
    }
}
