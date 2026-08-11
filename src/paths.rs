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

/// Where the daemon's stderr is written.
///
/// The daemon is spawned detached with its stdio discarded, so without this
/// a startup failure (a socket that won't bind, a port of the tool to a
/// platform where something is missing) left no trace anywhere — while the
/// CLI's own timeout message told the user to "check ~/.lsp-cli/logs/",
/// a directory nothing ever wrote to.
pub fn daemon_log_path() -> PathBuf {
    lsp_cli_home().join("logs").join("daemon.log")
}

/// Longest usable Unix Domain Socket path.
///
/// `sockaddr_un.sun_path` is a fixed-size array — 104 bytes on macOS and
/// the BSDs, 108 on Linux — and the path must fit with room for the NUL
/// terminator. It is not a `PATH_MAX`-sized field, which is why a socket in
/// a directory that is otherwise perfectly legal can fail to bind.
#[cfg(any(target_os = "macos", target_os = "ios", target_vendor = "apple"))]
const MAX_SOCKET_PATH_LEN: usize = 104;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_vendor = "apple")))]
const MAX_SOCKET_PATH_LEN: usize = 108;

/// Fails with an actionable message when the socket path is too long to
/// bind, instead of letting the daemon die silently and reporting it as a
/// startup timeout.
///
/// Reproduced on a macOS CI runner: `$TMPDIR` there is a ~49-character
/// `/var/folders/.../T/` path, so a state directory under it overflowed the
/// 104-byte limit. The daemon's `bind` failed, its stderr was discarded,
/// and every command spent the full `managerTimeout` before reporting
/// "daemon failed to start" — true, but useless.
pub fn check_socket_path(path: &std::path::Path) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len < MAX_SOCKET_PATH_LEN {
        return Ok(());
    }
    Err(format!(
        "socket path is {len} bytes, over this platform's {} byte limit for a Unix domain socket: {}\n\
         Set {HOME_ENV} to a shorter directory (the default, ~/.lsp-cli, is normally well under it).",
        MAX_SOCKET_PATH_LEN,
        path.display()
    ))
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
    fn a_normal_socket_path_is_accepted() {
        assert!(check_socket_path(std::path::Path::new("/home/u/.lsp-cli/manager.sock")).is_ok());
    }

    #[test]
    fn an_overlong_socket_path_is_rejected_with_an_actionable_message() {
        // The real macOS CI path that failed to bind: 112 bytes against a
        // 104-byte limit.
        let long = std::path::Path::new(
            "/var/folders/df/djsxfhc17x95674wsm_g8s980000gn/T/lsp-cli-test-bash_lang-4b54f702be9215c2-1786450808/manager.sock",
        );
        let err = check_socket_path(long).expect_err("should be rejected");
        assert!(err.contains("Unix domain socket"), "{err}");
        assert!(err.contains(HOME_ENV), "should say how to fix it: {err}");
    }

    #[test]
    fn the_boundary_leaves_room_for_the_nul_terminator() {
        let just_under = "a".repeat(MAX_SOCKET_PATH_LEN - 1);
        let exactly_at = "a".repeat(MAX_SOCKET_PATH_LEN);
        assert!(check_socket_path(std::path::Path::new(&just_under)).is_ok());
        assert!(check_socket_path(std::path::Path::new(&exactly_at)).is_err());
    }

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
