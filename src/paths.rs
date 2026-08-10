//! Where lsp-cli keeps its state on disk.
//!
//! Everything lives under one directory so a single environment variable
//! can relocate all of it. Five modules used to build
//! `dirs::home_dir().join(".lsp-cli")` independently, which meant there was
//! no way to point the tool somewhere else — and so the test suite operated
//! on the developer's real daemon, real socket, and real installed servers.
//! Tests that call `lsp server shutdown` were killing whatever the
//! developer had warm, and tests running in parallel fought over one global
//! daemon.

use std::path::PathBuf;

/// Overrides the state directory. Set by the test suite to give each test
/// binary its own daemon, socket, and config; also usable to run several
/// independent instances side by side.
pub const HOME_ENV: &str = "LSP_CLI_HOME";

/// Root of lsp-cli's state directory: `$LSP_CLI_HOME`, else `~/.lsp-cli`.
pub fn lsp_cli_home() -> PathBuf {
    if let Some(dir) = std::env::var_os(HOME_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir().unwrap_or_default().join(".lsp-cli")
}

/// Unix Domain Socket the manager daemon listens on.
pub fn socket_path() -> PathBuf {
    lsp_cli_home().join("manager.sock")
}

/// Lock file serializing daemon spawn across OS processes.
pub fn spawn_lock_path() -> PathBuf {
    lsp_cli_home().join("manager.spawn.lock")
}

/// User configuration file.
pub fn config_path() -> PathBuf {
    lsp_cli_home().join("config.json")
}

/// Where downloaded/npm-installed language servers are placed.
pub fn install_dir() -> PathBuf {
    lsp_cli_home().join("servers")
}

/// npm prefix for the Node-based servers.
pub fn packages_dir() -> PathBuf {
    lsp_cli_home().join("packages")
}

/// Isolated GOPATH for `go install`-managed servers.
pub fn go_dir() -> PathBuf {
    lsp_cli_home().join("go")
}

#[cfg(test)]
mod tests {
    use super::*;

    // These read a process-global env var, so they must not run
    // concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_override_relocates_every_path() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(HOME_ENV, "/tmp/lsp-cli-test-home");
        assert_eq!(lsp_cli_home(), PathBuf::from("/tmp/lsp-cli-test-home"));
        assert_eq!(
            socket_path(),
            PathBuf::from("/tmp/lsp-cli-test-home/manager.sock")
        );
        assert_eq!(
            install_dir(),
            PathBuf::from("/tmp/lsp-cli-test-home/servers")
        );
        std::env::remove_var(HOME_ENV);
    }

    #[test]
    fn without_the_override_it_falls_back_to_the_home_directory() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(HOME_ENV);
        assert!(lsp_cli_home().ends_with(".lsp-cli"));
    }

    #[test]
    fn an_empty_override_is_ignored_rather_than_using_the_filesystem_root() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(HOME_ENV, "");
        assert!(lsp_cli_home().ends_with(".lsp-cli"));
        std::env::remove_var(HOME_ENV);
    }
}
