// Not every test binary uses every helper here (each file under tests/ is
// compiled as its own crate against this shared module).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lsp"))
}

/// Isolated `LSP_CLI_HOME` for this test binary.
///
/// Every test used to run against the developer's real `~/.lsp-cli`, which
/// meant `lsp server shutdown` in one test killed whatever daemon the
/// developer (or another test binary running in parallel) had warm. Since
/// cargo runs test binaries concurrently and tests within a binary on
/// multiple threads, that produced exactly the "cannot reach manager
/// daemon" failures this suite kept hitting.
///
/// One home per test binary: tests inside a binary still share a daemon
/// (which is what most of them want — a warm server is expensive to start),
/// but no two binaries can disturb each other. Tests that specifically need
/// to shut the daemon down use [`isolated_home`] for a home of their own.
/// Root for test state directories.
///
/// Not `std::env::temp_dir()`: on macOS that is a ~49-byte
/// `/var/folders/<...>/T/` path, and a Unix domain socket path has a hard
/// 104-byte limit there (`sockaddr_un.sun_path`). A state directory under
/// it overflowed the limit, the daemon's `bind` failed, and every test in
/// the binary spent the full `managerTimeout` before reporting a startup
/// timeout — which is exactly what CI caught on macos-latest. `/tmp` is
/// five bytes and always present on the platforms this tool supports.
fn temp_root() -> PathBuf {
    PathBuf::from("/tmp")
}

fn shared_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        // Named after the test binary so parallel binaries never collide,
        // and stable across runs so warm servers survive between them —
        // but keyed on the `lsp` binary's mtime as well, so rebuilding the
        // tool starts a fresh daemon instead of leaving the previous
        // build's daemon serving stale behaviour. (Debugging that
        // once is enough.)
        let name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "unknown".into());
        let build = std::fs::metadata(bin_path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Short by construction, for the socket-path limit above: the
        // binary name is already unique per test binary, and the build
        // stamp only needs enough entropy to change when `lsp` is rebuilt.
        let dir = temp_root().join(format!("lsp-t-{name}-{:x}", build));
        prepare_home(&dir);
        dir
    })
}

/// Creates a state directory that can find the developer's real installed
/// language servers.
///
/// The servers directory is linked rather than copied: pointing
/// `LSP_CLI_HOME` at an empty temp dir would make every `has_*_server()`
/// gate report "not installed", silently skipping the entire
/// language-specific suite instead of isolating it.
fn prepare_home(dir: &Path) {
    std::fs::create_dir_all(dir).expect("failed to create test LSP_CLI_HOME");
    for shared in ["servers", "packages", "go"] {
        let real = dirs::home_dir()
            .unwrap_or_default()
            .join(".lsp-cli")
            .join(shared);
        let link = dir.join(shared);
        if real.exists() && !link.exists() {
            let _ = std::os::unix::fs::symlink(&real, &link);
        }
    }
}

/// A private state directory, removed when the returned guard drops. For
/// tests that start, kill, or shut down a daemon and would otherwise
/// disturb their neighbours.
pub struct IsolatedHome {
    dir: PathBuf,
}

impl IsolatedHome {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        // Best effort: stop the daemon this home owns, then remove it.
        let _ = Command::new(bin_path())
            .args(["server", "shutdown"])
            .env("LSP_CLI_HOME", &self.dir)
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn isolated_home(label: &str) -> IsolatedHome {
    let dir = temp_root().join(format!(
        "lsp-i-{label}-{}-{}",
        std::process::id(),
        // Distinguishes concurrent tests in the same process without
        // needing a random source.
        NEXT_HOME_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    prepare_home(&dir);
    IsolatedHome { dir }
}

/// Like [`isolated_home`], but with no link to the developer's installed
/// servers: for tests that need "nothing is installed" to be true.
pub fn empty_home(label: &str) -> IsolatedHome {
    let dir = temp_root().join(format!(
        "lsp-e-{label}-{}-{}",
        std::process::id(),
        NEXT_HOME_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create empty LSP_CLI_HOME");
    IsolatedHome { dir }
}

static NEXT_HOME_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runs `lsp` against `home` with extra environment variables (e.g. a
/// `PATH` holding fake tools).
pub fn lsp_in_env(home: &IsolatedHome, args: &[&str], envs: &[(&str, &str)]) -> RunResult {
    let output = Command::new(bin_path())
        .args(args)
        .env("LSP_CLI_HOME", home.path())
        .envs(envs.iter().copied())
        .output()
        .expect("failed to execute lsp binary");
    RunResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code().unwrap_or(1),
    }
}

/// Writes an executable `#!/bin/sh` script called `name` into `dir`.
pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub fn lsp(args: &[&str]) -> RunResult {
    run_with_home(shared_home(), args)
}

/// Runs `lsp` against a specific state directory.
pub fn lsp_in(home: &IsolatedHome, args: &[&str]) -> RunResult {
    run_with_home(home.path(), args)
}

fn run_with_home(home: &Path, args: &[&str]) -> RunResult {
    let output = Command::new(bin_path())
        .args(args)
        .env("LSP_CLI_HOME", home)
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

/// Absolute path of `name` on `PATH`, if any.
pub fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("which").arg(name).output().ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
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

/// json's, css's, and html's servers are bundled and built as sibling
/// binaries of `lsp` itself (see Cargo.toml's `[[bin]]` entries and
/// src/servers/) rather than optional external dependencies — `cargo
/// test`/`cargo build` building the package builds them too, so this is
/// really just checking "did the build actually produce it," not "is it
/// installed."
fn has_bundled_server(bin_name: &str) -> bool {
    bin_path()
        .parent()
        .map(|d| d.join(bin_name).exists())
        .unwrap_or(false)
}

pub fn has_html_server() -> bool {
    has_bundled_server("lsp-html-lsp")
}

pub fn has_css_server() -> bool {
    has_bundled_server("lsp-css-lsp")
}

pub fn has_json_server() -> bool {
    has_bundled_server("lsp-json-lsp")
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
    has_bundled_server("lsp-bash-lsp")
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

/// A project served by `tests/support/fake_lsp.py`, installed as the zig
/// server (`zls`) on a private `PATH`. For tests that need a server whose
/// behaviour they control. `envs()` gives the environment every `lsp`
/// invocation against it must use.
pub struct FakeServerProject {
    pub home: IsolatedHome,
    pub dir: PathBuf,
    pub markers: PathBuf,
    path: String,
    extra: Vec<(String, String)>,
}

impl FakeServerProject {
    pub fn new(label: &str, source: &str, extra_env: &[(&str, &str)]) -> Self {
        let home = empty_home(label);
        let dir = home.path().join("proj");
        let fakebin = home.path().join("fakebin");
        let markers = home.path().join("markers");
        for d in [&dir, &fakebin, &markers] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(dir.join("build.zig"), "").unwrap();
        std::fs::write(dir.join("main.zig"), source).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_lsp.py");
        write_script(
            &fakebin,
            "zls",
            &format!("exec python3 \"{}\" \"$@\"", script.display()),
        );
        let path = format!("{}:/usr/bin:/bin", fakebin.display());
        let mut extra: Vec<(String, String)> = extra_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        extra.push(("FAKE_LSP_MARKER_DIR".into(), markers.display().to_string()));
        FakeServerProject {
            home,
            dir,
            markers,
            path,
            extra,
        }
    }

    pub fn file(&self) -> String {
        self.dir.join("main.zig").display().to_string()
    }

    pub fn run(&self, args: &[&str]) -> RunResult {
        let mut envs: Vec<(&str, &str)> = vec![("PATH", self.path.as_str())];
        envs.extend(self.extra.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        lsp_in_env(&self.home, args, &envs)
    }
}
