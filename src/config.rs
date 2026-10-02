use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspCliConfig {
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    #[serde(default = "default_manager_timeout")]
    pub manager_timeout: u64,
    #[serde(default = "default_max_items")]
    pub default_max_items: usize,
    /// Use a working language server already on `PATH` when none is in the
    /// managed install directory, instead of downloading one.
    #[serde(default = "default_use_path_servers")]
    pub use_path_servers: bool,
    /// Milliseconds to pause after a document change before querying a
    /// (non-bundled) server. See `commands::settle_delay`.
    #[serde(default)]
    pub settle_ms: u64,
}

fn default_idle_timeout() -> u64 {
    // 10 minutes. Was 300s (matching the TS original's default) until the
    // navigation commands started routing through the daemon and reusing
    // warm servers across invocations/processes instead of spawning fresh
    // per call — a longer default keeps a server alive across a realistic
    // gap between commands in an interactive session. Still overridable via
    // `idleTimeout` in `~/.lsp-cli/config.json`.
    600
}
fn default_manager_timeout() -> u64 {
    60
}
fn default_max_items() -> usize {
    20
}
fn default_use_path_servers() -> bool {
    true
}

impl Default for LspCliConfig {
    fn default() -> Self {
        Self {
            idle_timeout: default_idle_timeout(),
            manager_timeout: default_manager_timeout(),
            default_max_items: default_max_items(),
            use_path_servers: default_use_path_servers(),
            settle_ms: 0,
        }
    }
}

pub use crate::paths::config_path;

/// Load config, merging user overrides over defaults. Never errors: falls back
/// to defaults on missing file or parse failure, matching utils/config.ts.
pub fn load_config() -> LspCliConfig {
    load_config_from(&config_path())
}

/// The actual file-reading/merging logic, factored out of `load_config` so
/// it can be exercised directly against a real (tempfile) path in tests
/// instead of only indirectly through the fixed `~/.lsp-cli/config.json`
/// location (which tests can't safely share/mutate concurrently).
fn load_config_from(path: &std::path::Path) -> LspCliConfig {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return LspCliConfig::default();
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .map(|v| {
            let mut cfg = LspCliConfig::default();
            if let Some(n) = v.get("idleTimeout").and_then(|x| x.as_u64()) {
                cfg.idle_timeout = n;
            }
            if let Some(n) = v.get("managerTimeout").and_then(|x| x.as_u64()) {
                cfg.manager_timeout = n;
            }
            if let Some(n) = v.get("defaultMaxItems").and_then(|x| x.as_u64()) {
                cfg.default_max_items = n as usize;
            }
            if let Some(b) = v.get("usePathServers").and_then(|x| x.as_bool()) {
                cfg.use_path_servers = b;
            }
            if let Some(n) = v.get("settleMs").and_then(|x| x.as_u64()) {
                cfg.settle_ms = n;
            }
            cfg
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_ts() {
        let c = LspCliConfig::default();
        assert_eq!(c.idle_timeout, 600);
        assert_eq!(c.manager_timeout, 60);
        assert_eq!(c.default_max_items, 20);
    }

    #[test]
    fn parses_partial_overrides() {
        let v: serde_json::Value = serde_json::json!({ "idleTimeout": 42 });
        let raw = v.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.get("idleTimeout").and_then(|x| x.as_u64()), Some(42));
    }

    // --- load_config_from: the actual file-reading function, previously
    // untested end-to-end (the two tests above only exercised `Default` and
    // generic serde_json round-tripping, not this module's own logic).

    #[test]
    fn use_path_servers_defaults_on_and_can_be_turned_off() {
        assert!(LspCliConfig::default().use_path_servers);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"usePathServers": false}"#).unwrap();
        assert!(!load_config_from(&path).use_path_servers);
        // A wrong type is ignored, not a reason to flip the default.
        std::fs::write(&path, r#"{"usePathServers": "no"}"#).unwrap();
        assert!(load_config_from(&path).use_path_servers);
    }

    #[test]
    fn settle_ms_defaults_to_zero_and_is_read_from_the_file() {
        assert_eq!(LspCliConfig::default().settle_ms, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"settleMs": 250}"#).unwrap();
        assert_eq!(load_config_from(&path).settle_ms, 250);
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let cfg = load_config_from(&path);
        assert_eq!(cfg.idle_timeout, 600);
        assert_eq!(cfg.manager_timeout, 60);
        assert_eq!(cfg.default_max_items, 20);
    }

    #[test]
    fn malformed_json_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not valid json ").unwrap();
        let cfg = load_config_from(&path);
        assert_eq!(cfg.idle_timeout, 600);
    }

    #[test]
    fn partial_override_file_merges_over_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"idleTimeout": 42}"#).unwrap();
        let cfg = load_config_from(&path);
        assert_eq!(cfg.idle_timeout, 42);
        // Untouched fields keep their defaults rather than zeroing out.
        assert_eq!(cfg.manager_timeout, 60);
        assert_eq!(cfg.default_max_items, 20);
    }

    #[test]
    fn full_override_file_replaces_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"idleTimeout": 1, "managerTimeout": 2, "defaultMaxItems": 3}"#,
        )
        .unwrap();
        let cfg = load_config_from(&path);
        assert_eq!(cfg.idle_timeout, 1);
        assert_eq!(cfg.manager_timeout, 2);
        assert_eq!(cfg.default_max_items, 3);
    }

    #[test]
    fn unknown_fields_in_the_file_are_ignored_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"idleTimeout": 99, "somethingLspCliDoesNotKnowAbout": true}"#,
        )
        .unwrap();
        let cfg = load_config_from(&path);
        assert_eq!(cfg.idle_timeout, 99);
    }
}
