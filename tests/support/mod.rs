// Not every test binary uses every helper here (each file under tests/ is
// compiled as its own crate against this shared module).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lsp"))
}

pub fn lsp(args: &[&str]) -> RunResult {
    let output = Command::new(bin_path())
        .args(args)
        .output()
        .expect("failed to execute lsp binary");
    RunResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(1),
    }
}

pub fn lsp_json(args: &[&str]) -> serde_json::Value {
    let mut full = args.to_vec();
    full.push("--output");
    full.push("json");
    let result = lsp(&full);
    assert_eq!(
        result.exit_code, 0,
        "lsp {:?} exited {}: {}",
        full, result.exit_code, result.stderr
    );
    serde_json::from_str(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON from lsp {:?}: {e}\nstdout: {}",
            full, result.stdout
        )
    })
}

pub fn has_binary(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn lsp_cli_servers_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".lsp-cli")
        .join("servers")
}

fn has_server(bin_name: &str) -> bool {
    has_binary(bin_name) || lsp_cli_servers_dir().join(bin_name).exists()
}

pub fn has_ts_server() -> bool {
    has_server("typescript-language-server")
}

pub fn has_gopls() -> bool {
    has_server("gopls")
}

pub fn has_basedpyright() -> bool {
    has_server("basedpyright-langserver")
}

pub fn has_rust_analyzer() -> bool {
    has_server("rust-analyzer")
}

pub fn has_html_server() -> bool {
    has_server("vscode-html-language-server")
}

pub fn has_css_server() -> bool {
    has_server("vscode-css-language-server")
}

pub fn has_json_server() -> bool {
    has_server("vscode-json-language-server")
}

pub fn has_jdtls() -> bool {
    has_server("jdtls")
}

pub fn has_kotlin_language_server() -> bool {
    has_server("kotlin/server/bin/kotlin-language-server") || has_binary("kotlin-language-server")
}

pub fn has_clangd() -> bool {
    has_server("clangd/bin/clangd") || has_binary("clangd")
}

pub fn has_lua_language_server() -> bool {
    has_server("lua/bin/lua-language-server") || has_binary("lua-language-server")
}

pub fn has_zls() -> bool {
    has_server("zls") || has_binary("zls")
}

pub fn has_bash_language_server() -> bool {
    has_server("bash-language-server") || has_binary("bash-language-server")
}

pub fn has_csharp_ls() -> bool {
    has_server("csharp-ls") || has_binary("csharp-ls")
}

// ruby-lsp additionally needs a working `bundle` on PATH to compose its
// per-project bundle on startup (see CONTRIBUTING.md's Ruby setup section) —
// checking for the binary alone isn't enough to predict whether it'll
// actually start.
pub fn has_ruby_lsp() -> bool {
    (has_server("ruby-lsp") || has_binary("ruby-lsp")) && has_binary("bundle")
}

pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

pub fn ts_fixture(rel: &str) -> PathBuf {
    fixture(&format!("typescript_project/{rel}"))
}

pub fn py_fixture(rel: &str) -> PathBuf {
    fixture(&format!("python_project/{rel}"))
}

pub fn go_fixture(rel: &str) -> PathBuf {
    fixture(&format!("go_project/{rel}"))
}

pub fn rust_fixture(rel: &str) -> PathBuf {
    fixture(&format!("rust_project/{rel}"))
}

pub fn web_fixture(rel: &str) -> PathBuf {
    fixture(&format!("web_project/{rel}"))
}

pub fn cpp_fixture(rel: &str) -> PathBuf {
    fixture(&format!("cpp_project/{rel}"))
}

pub fn lua_fixture(rel: &str) -> PathBuf {
    fixture(&format!("lua_project/{rel}"))
}

pub fn zig_fixture(rel: &str) -> PathBuf {
    fixture(&format!("zig_project/{rel}"))
}

pub fn bash_fixture(rel: &str) -> PathBuf {
    fixture(&format!("bash_project/{rel}"))
}

pub fn csharp_fixture(rel: &str) -> PathBuf {
    fixture(&format!("csharp_project/{rel}"))
}

pub fn ruby_fixture(rel: &str) -> PathBuf {
    fixture(&format!("ruby_project/{rel}"))
}
