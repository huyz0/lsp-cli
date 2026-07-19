//! Automatic language server installation. npm-installed servers
//! (typescript, python, html/css/json, bash) get a thin shell wrapper into
//! `~/.lsp-cli/servers/<bin>` that execs `node <entry> "$@"`; gopls is
//! `go install`ed into an isolated GOPATH and symlinked in; rust-analyzer,
//! kotlin-language-server, clangd, lua-language-server, and zls are fetched
//! from GitHub Releases; jdtls is fetched from Eclipse's downloads and
//! wrapped in a script that pins in a JDK it finds via
//! sdkman/`JAVA_HOME`/`PATH` (installing a JDK itself is out of scope —
//! it's a much bigger, more opinionated dependency than any other managed
//! server); csharp-ls and ruby-lsp go through `dotnet tool install`/`gem
//! install` respectively. deno remains unmanaged since it relies on the
//! `deno` binary already being on `PATH`.

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::registry::default_install_dir;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn packages_dir() -> PathBuf {
    home().join(".lsp-cli").join("packages")
}

fn go_dir() -> PathBuf {
    home().join(".lsp-cli").join("go")
}

/// Managed languages, in the same order `lsp install list` should show them.
pub const MANAGED_LANGUAGES: &[&str] =
    &["typescript", "python", "go", "rust", "java", "kotlin", "html", "css", "json", "cpp", "lua", "zig", "csharp", "ruby", "bash"];

fn is_managed(language: &str) -> bool {
    MANAGED_LANGUAGES.contains(&language)
}

/// Writes a `#!/bin/sh` wrapper at `wrapper_path` that execs `node <entry> "$@"`.
fn write_node_wrapper(wrapper_path: &Path, entry: &Path) -> Result<()> {
    let script = format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", entry.display());
    std::fs::write(wrapper_path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(wrapper_path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn npm_install(packages: &[&str]) -> Result<()> {
    let dir = packages_dir();
    std::fs::create_dir_all(&dir)?;
    let status = Command::new("npm").arg("install").args(packages).current_dir(&dir).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => bail!("npm install {} failed with exit code {:?}", packages.join(" "), s.code()),
        Err(e) => bail!("failed to run npm (is it installed and on PATH?): {e}"),
    }
}

fn run_binary_version(bin: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// typescript, python, html/css/json — all npm packages with
// a node-script entry point, wrapped identically.
// ---------------------------------------------------------------------------

struct NpmSpec {
    packages: &'static [&'static str],
    entry_rel: &'static str,
    wrapper_name: &'static str,
    version_args: &'static [&'static str],
    /// Some LSP entry points (basedpyright's `langserver.index.js`, every
    /// vscode-*-language-server bin) don't support `--version` at all — they
    /// just start the LSP loop and error out waiting for a stdio handshake.
    /// When set, this alternate entry (also from the same npm package) is
    /// used only for the version probe, not for actually running the server.
    version_entry_rel: Option<&'static str>,
}

fn npm_spec(language: &str) -> Option<NpmSpec> {
    Some(match language {
        "typescript" => NpmSpec {
            packages: &["typescript-language-server", "typescript"],
            entry_rel: "node_modules/typescript-language-server/lib/cli.mjs",
            wrapper_name: "typescript-language-server",
            version_args: &["--version"],
            version_entry_rel: None,
        },
        "python" => NpmSpec {
            packages: &["basedpyright"],
            entry_rel: "node_modules/basedpyright/langserver.index.js",
            wrapper_name: "basedpyright-langserver",
            version_args: &["--version"],
            version_entry_rel: Some("node_modules/basedpyright/index.js"),
        },
        "html" => NpmSpec {
            packages: &["vscode-langservers-extracted"],
            entry_rel: "node_modules/vscode-langservers-extracted/bin/vscode-html-language-server",
            wrapper_name: "vscode-html-language-server",
            version_args: &["--version"],
            version_entry_rel: None,
        },
        "css" => NpmSpec {
            packages: &["vscode-langservers-extracted"],
            entry_rel: "node_modules/vscode-langservers-extracted/bin/vscode-css-language-server",
            wrapper_name: "vscode-css-language-server",
            version_args: &["--version"],
            version_entry_rel: None,
        },
        "json" => NpmSpec {
            packages: &["vscode-langservers-extracted"],
            entry_rel: "node_modules/vscode-langservers-extracted/bin/vscode-json-language-server",
            wrapper_name: "vscode-json-language-server",
            version_args: &["--version"],
            version_entry_rel: None,
        },
        "bash" => NpmSpec {
            packages: &["bash-language-server"],
            entry_rel: "node_modules/bash-language-server/out/cli.js",
            wrapper_name: "bash-language-server",
            version_args: &["--version"],
            version_entry_rel: None,
        },
        _ => return None,
    })
}

fn install_npm(spec: &NpmSpec) -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Installing {} (npm install {})...", spec.wrapper_name, spec.packages.join(" "));
    npm_install(spec.packages)?;

    let entry = packages_dir().join(spec.entry_rel);
    if !entry.exists() {
        bail!("npm install succeeded but expected entry point is missing: {}", entry.display());
    }
    let wrapper = install_dir.join(spec.wrapper_name);
    write_node_wrapper(&wrapper, &entry)?;
    println!("\u{2713} Installed to {}", wrapper.display());
    Ok(wrapper)
}

fn check_npm_version(spec: &NpmSpec) -> Option<String> {
    let wrapper = default_install_dir().join(spec.wrapper_name);
    if !wrapper.exists() {
        return None;
    }
    let entry = packages_dir().join(spec.entry_rel);
    if !entry.exists() {
        return None;
    }

    let version_entry = spec.version_entry_rel.map(|rel| packages_dir().join(rel)).unwrap_or_else(|| entry.clone());
    if let Some(text) = version_entry
        .to_str()
        .filter(|_| version_entry.exists())
        .and_then(|e| run_binary_version(&PathBuf::from("node"), &[e, spec.version_args[0]]))
    {
        return Some(text);
    }

    // Some LSP entries (every vscode-*-language-server bin) don't support
    // `--version` at all and no alternate entry exists to probe instead.
    // Treat "the entry point is present" as "installed" rather than
    // re-running the install on every invocation.
    Some("installed".to_string())
}

// ---------------------------------------------------------------------------
// go — gopls via `go install`
// ---------------------------------------------------------------------------

fn install_go() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Installing gopls via go install...");
    let gopath = go_dir();
    std::fs::create_dir_all(&gopath)?;
    let status = Command::new("go")
        .args(["install", "golang.org/x/tools/gopls@latest"])
        .env("GOPATH", &gopath)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => bail!("go install failed with exit code {:?}", s.code()),
        Err(e) => bail!("failed to run go (is it installed and on PATH?): {e}"),
    }

    let src = gopath.join("bin").join("gopls");
    if !src.exists() {
        bail!("go install succeeded but gopls binary is missing at {}", src.display());
    }
    let dest = install_dir.join("gopls");
    if dest.exists() || dest.symlink_metadata().is_ok() {
        std::fs::remove_file(&dest).ok();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&src, &dest)?;
    #[cfg(not(unix))]
    std::fs::copy(&src, &dest)?;
    println!("\u{2713} Installed to {}", dest.display());
    Ok(dest)
}

fn check_go_version() -> Option<String> {
    let bin = default_install_dir().join("gopls");
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["version"]).map(|v| v.lines().next().unwrap_or(&v).to_string())
}

// ---------------------------------------------------------------------------
// rust — rust-analyzer from GitHub Releases
// ---------------------------------------------------------------------------

fn rust_analyzer_target() -> Result<(&'static str, &'static str)> {
    rust_analyzer_target_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Pure, parameterized so every supported (and unsupported) OS/arch
/// combination can be unit tested without depending on the machine actually
/// running the tests.
fn rust_analyzer_target_for(os: &str, arch: &str) -> Result<(&'static str, &'static str)> {
    // Returns (release-asset target triple, file extension including dot).
    let target = match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => bail!("Unsupported OS/arch for rust-analyzer: {os}-{arch}"),
    };
    let ext = if os == "windows" { ".zip" } else { ".gz" };
    Ok((target, ext))
}

#[derive(serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

async fn fetch_latest_release(repo: &str) -> Result<GithubRelease> {
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .header("User-Agent", "lsp-cli")
        .send()
        .await
        .map_err(|e| anyhow!("failed to reach GitHub API ({url}): {e}"))?;
    if !resp.status().is_success() {
        bail!("Failed to fetch latest release for {repo}: HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// A fresh, unpredictable directory under the system temp dir for staging a
/// single download. `create_dir` (unlike `fs::write`) fails if the path
/// already exists instead of following it — including if it's a symlink an
/// attacker pre-planted at a predictable `temp_dir().join(filename)` path to
/// redirect our write to an arbitrary file. Using a per-download directory
/// (rather than hardening just the filename) also means the temp gunzip/
/// unzip inputs and outputs can't collide with any other concurrent install.
fn unique_temp_dir() -> Result<PathBuf> {
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("lsp-cli-install-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| anyhow!("failed to create temp install dir {}: {e}", dir.display()))?;
    Ok(dir)
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let resp = client.get(url).header("User-Agent", "lsp-cli").send().await?;
    if !resp.status().is_success() {
        bail!("Download failed: HTTP {} for {url}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

async fn install_rust_analyzer() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching rust-analyzer from GitHub Releases...");

    let (target, ext) = rust_analyzer_target()?;
    let filename = format!("rust-analyzer-{target}{ext}");

    let release = fetch_latest_release("rust-lang/rust-analyzer").await?;
    let asset = release.assets.iter().find(|a| a.name == filename).ok_or_else(|| anyhow!("Could not find release asset {filename}"))?;

    println!("Downloading {filename}...");
    let bytes = download_bytes(&asset.browser_download_url).await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join(&filename);
    std::fs::write(&temp_path, &bytes)?;

    let dest_name = if std::env::consts::OS == "windows" { "rust-analyzer.exe" } else { "rust-analyzer" };
    let dest = install_dir.join(dest_name);

    if ext == ".gz" {
        let output = Command::new("gunzip").arg("-c").arg(&temp_path).output()?;
        if !output.status.success() {
            bail!("gunzip failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::fs::write(&dest, &output.stdout)?;
    } else {
        let output = Command::new("unzip").arg("-p").arg(&temp_path).output()?;
        if !output.status.success() {
            bail!("unzip failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::fs::write(&dest, &output.stdout)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::remove_dir_all(&temp_dir).ok();
    println!("\u{2713} Installed to {}", dest.display());
    Ok(dest)
}

fn check_rust_analyzer_version() -> Option<String> {
    let dest_name = if std::env::consts::OS == "windows" { "rust-analyzer.exe" } else { "rust-analyzer" };
    let bin = default_install_dir().join(dest_name);
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["--version"])
}

// ---------------------------------------------------------------------------
// java — Eclipse JDT Language Server, requires a JDK already present
// (via sdkman, JAVA_HOME, or PATH). Unlike the other managed servers this
// one is never fetched as a standalone binary — it ships as an OSGi bundle
// that must be launched with `java -jar <launcher> -configuration <dir>`, so
// installing it means finding a JDK, downloading+extracting the bundle, and
// writing a wrapper script that pins in the resolved java/launcher/config
// paths.
// ---------------------------------------------------------------------------

/// Where the extracted jdtls bundle (plugins/, config_*/, etc.) lives.
/// Deliberately not `servers/jdtls` — that path is the wrapper *script*
/// (`install_dir.join("jdtls")`, matching `server_bin` in the registry), and
/// writing a file over an existing directory (or vice versa) fails.
fn jdtls_install_dir() -> PathBuf {
    default_install_dir().join("jdtls-dist")
}

/// Looks for a JDK in the order a JVM developer would expect: an active
/// sdkman-managed version first (since the user may have multiple JDKs
/// installed and `sdk use`/`sdk default` is how they pick one), then
/// `JAVA_HOME`, then whatever `java` resolves to on `PATH`.
fn find_java() -> Option<PathBuf> {
    let sdkman_java = home().join(".sdkman").join("candidates").join("java").join("current").join("bin").join("java");
    if sdkman_java.exists() {
        return Some(sdkman_java);
    }
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let candidate = PathBuf::from(java_home).join("bin").join("java");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let on_path = Command::new("java").arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
    on_path.then(|| PathBuf::from("java"))
}

fn jdtls_config_dir_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "config_mac",
        "windows" => "config_win",
        _ => "config_linux",
    }
}

fn find_launcher_jar(jdtls_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(jdtls_dir.join("plugins")).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("org.eclipse.equinox.launcher_") && n.ends_with(".jar"))
            .unwrap_or(false)
    })
}

fn write_jdtls_wrapper(wrapper_path: &Path, java: &Path, launcher: &Path, config_dir: &Path) -> Result<()> {
    let script = format!(
        "#!/bin/sh\nexec \"{}\" \\\n  -Declipse.application=org.eclipse.jdt.ls.core.id1 \\\n  -Dosgi.bundles.defaultStartLevel=4 \\\n  -Declipse.product=org.eclipse.jdt.ls.core.product \\\n  -Dlog.level=ALL \\\n  -Xmx1G \\\n  --add-modules=ALL-SYSTEM \\\n  --add-opens java.base/java.util=ALL-UNNAMED \\\n  --add-opens java.base/java.lang=ALL-UNNAMED \\\n  -jar \"{}\" \\\n  -configuration \"{}\" \\\n  \"$@\"\n",
        java.display(),
        launcher.display(),
        config_dir.display()
    );
    std::fs::write(wrapper_path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(wrapper_path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

async fn install_jdtls() -> Result<PathBuf> {
    let java = find_java().ok_or_else(|| {
        anyhow!(
            "No JDK found (checked ~/.sdkman/candidates/java/current, $JAVA_HOME, and `java` on PATH).\n\
             Install one first, e.g. via sdkman: `sdk install java`, then retry."
        )
    })?;

    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching Eclipse JDT Language Server (using JDK at {})...", java.display());

    let bytes = download_bytes("https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz").await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join("jdt-language-server-latest.tar.gz");
    std::fs::write(&temp_path, &bytes)?;

    let dest = jdtls_install_dir();
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;
    let output = Command::new("tar").arg("-xzf").arg(&temp_path).arg("-C").arg(&dest).output()?;
    if !output.status.success() {
        bail!("Failed to extract jdtls: {}", String::from_utf8_lossy(&output.stderr));
    }
    std::fs::remove_dir_all(&temp_dir).ok();

    let launcher = find_launcher_jar(&dest).ok_or_else(|| anyhow!("jdtls archive extracted but no launcher jar found under {}", dest.join("plugins").display()))?;
    let config_dir = dest.join(jdtls_config_dir_name());
    if !config_dir.exists() {
        bail!("jdtls archive extracted but expected config dir is missing: {}", config_dir.display());
    }

    let wrapper = install_dir.join("jdtls");
    write_jdtls_wrapper(&wrapper, &java, &launcher, &config_dir)?;
    println!("\u{2713} Installed to {}", wrapper.display());
    Ok(wrapper)
}

fn check_jdtls_version() -> Option<String> {
    let wrapper = default_install_dir().join("jdtls");
    let dest = jdtls_install_dir();
    (wrapper.exists() && find_launcher_jar(&dest).is_some()).then(|| "installed".to_string())
}

// ---------------------------------------------------------------------------
// kotlin — kotlin-language-server from GitHub Releases (zip)
// ---------------------------------------------------------------------------

fn kotlin_server_bin(install_dir: &Path) -> PathBuf {
    let name = if std::env::consts::OS == "windows" { "kotlin-language-server.bat" } else { "kotlin-language-server" };
    install_dir.join("kotlin").join("server").join("bin").join(name)
}

async fn install_kotlin() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching kotlin-language-server from GitHub Releases...");

    let filename = "server.zip";
    let release = fetch_latest_release("fwcd/kotlin-language-server").await?;
    let asset = release.assets.iter().find(|a| a.name == filename).ok_or_else(|| anyhow!("Could not find release asset {filename}"))?;

    println!("Downloading {filename}...");
    let bytes = download_bytes(&asset.browser_download_url).await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join(filename);
    std::fs::write(&temp_path, &bytes)?;

    let dest = install_dir.join("kotlin");
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;

    let output = Command::new("unzip").args(["-q", "-o"]).arg(&temp_path).arg("-d").arg(&dest).output()?;
    if !output.status.success() {
        bail!("Failed to unzip kotlin-language-server: {}", String::from_utf8_lossy(&output.stderr));
    }

    let server_bin = kotlin_server_bin(&install_dir);
    if server_bin.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&server_bin, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    std::fs::remove_dir_all(&temp_dir).ok();
    println!("\u{2713} Installed to {}", server_bin.display());
    Ok(server_bin)
}

fn check_kotlin_version() -> Option<String> {
    let bin = kotlin_server_bin(&default_install_dir());
    bin.exists().then(|| "installed".to_string())
}

// ---------------------------------------------------------------------------
// cpp — clangd from GitHub Releases (zip, wraps a version-stamped top-level
// directory we normalize away so the registry can reference a fixed path)
// ---------------------------------------------------------------------------

fn clangd_asset_name(version: &str) -> Result<String> {
    match std::env::consts::OS {
        "linux" => Ok(format!("clangd-linux-{version}.zip")),
        "macos" => Ok(format!("clangd-mac-{version}.zip")),
        other => bail!("Unsupported OS for clangd: {other}"),
    }
}

fn clangd_server_bin() -> PathBuf {
    PathBuf::from("clangd").join("bin").join("clangd")
}

async fn install_clangd() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching clangd from GitHub Releases...");

    let release = fetch_latest_release("clangd/clangd").await?;
    let version = release.tag_name.clone();
    let filename = clangd_asset_name(&version)?;
    let asset = release.assets.iter().find(|a| a.name == filename).ok_or_else(|| anyhow!("Could not find release asset {filename}"))?;

    println!("Downloading {filename}...");
    let bytes = download_bytes(&asset.browser_download_url).await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join(&filename);
    std::fs::write(&temp_path, &bytes)?;

    // Extracted under `install_dir` itself (not the system temp dir) so
    // the final `rename` below stays on one filesystem — renaming across
    // filesystems (e.g. a tmpfs `/tmp` vs. `~/.lsp-cli` on a different
    // mount) fails with `EXDEV`, reproduced live during review.
    let extract_dir = install_dir.join(".clangd-extract");
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir)?;
    }
    std::fs::create_dir_all(&extract_dir)?;
    let output = Command::new("unzip").args(["-q", "-o"]).arg(&temp_path).arg("-d").arg(&extract_dir).output()?;
    if !output.status.success() {
        bail!("Failed to unzip clangd: {}", String::from_utf8_lossy(&output.stderr));
    }
    std::fs::remove_dir_all(&temp_dir).ok();

    // The zip wraps everything in a version-stamped `clangd_<version>/`
    // directory; strip that so the registry can reference a fixed
    // `clangd/bin/clangd` path regardless of installed version.
    let unpacked = std::fs::read_dir(&extract_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .ok_or_else(|| anyhow!("clangd archive did not contain the expected top-level directory"))?
        .path();

    let dest = install_dir.join("clangd");
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(&unpacked, &dest)?;
    std::fs::remove_dir_all(&extract_dir).ok();

    let bin = install_dir.join(clangd_server_bin());
    if !bin.exists() {
        bail!("clangd archive extracted but binary is missing at {}", bin.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn check_clangd_version() -> Option<String> {
    let bin = default_install_dir().join(clangd_server_bin());
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["--version"])
}

// ---------------------------------------------------------------------------
// lua — lua-language-server from GitHub Releases (tar.gz, unpacks flat with
// no wrapping directory)
// ---------------------------------------------------------------------------

fn lua_language_server_asset_name(version: &str) -> Result<String> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("macos", "aarch64") => "darwin-arm64",
        (os, arch) => bail!("Unsupported OS/arch for lua-language-server: {os}-{arch}"),
    };
    Ok(format!("lua-language-server-{version}-{platform}.tar.gz"))
}

fn lua_language_server_bin() -> PathBuf {
    PathBuf::from("lua").join("bin").join("lua-language-server")
}

async fn install_lua_language_server() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching lua-language-server from GitHub Releases...");

    let release = fetch_latest_release("LuaLS/lua-language-server").await?;
    let version = release.tag_name.clone();
    let filename = lua_language_server_asset_name(&version)?;
    let asset = release.assets.iter().find(|a| a.name == filename).ok_or_else(|| anyhow!("Could not find release asset {filename}"))?;

    println!("Downloading {filename}...");
    let bytes = download_bytes(&asset.browser_download_url).await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join(&filename);
    std::fs::write(&temp_path, &bytes)?;

    let dest = install_dir.join("lua");
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;
    let output = Command::new("tar").arg("-xzf").arg(&temp_path).arg("-C").arg(&dest).output()?;
    if !output.status.success() {
        bail!("Failed to extract lua-language-server: {}", String::from_utf8_lossy(&output.stderr));
    }
    std::fs::remove_dir_all(&temp_dir).ok();

    let bin = install_dir.join(lua_language_server_bin());
    if !bin.exists() {
        bail!("lua-language-server archive extracted but binary is missing at {}", bin.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn check_lua_language_server_version() -> Option<String> {
    let bin = default_install_dir().join(lua_language_server_bin());
    bin.exists().then(|| "installed".to_string())
}

// ---------------------------------------------------------------------------
// zig — zls from GitHub Releases (a bare binary at the archive root, no
// wrapping directory to strip)
// ---------------------------------------------------------------------------

fn zls_asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("zls-x86_64-linux.tar.xz"),
        ("linux", "aarch64") => Ok("zls-aarch64-linux.tar.xz"),
        ("macos", "x86_64") => Ok("zls-x86_64-macos.tar.xz"),
        ("macos", "aarch64") => Ok("zls-aarch64-macos.tar.xz"),
        (os, arch) => bail!("Unsupported OS/arch for zls: {os}-{arch}"),
    }
}

async fn install_zls() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching zls from GitHub Releases...");

    let filename = zls_asset_name()?;
    let release = fetch_latest_release("zigtools/zls").await?;
    let asset = release.assets.iter().find(|a| a.name == filename).ok_or_else(|| anyhow!("Could not find release asset {filename}"))?;

    println!("Downloading {filename}...");
    let bytes = download_bytes(&asset.browser_download_url).await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join(filename);
    std::fs::write(&temp_path, &bytes)?;

    let output = Command::new("tar").arg("-xJf").arg(&temp_path).arg("-C").arg(&install_dir).output()?;
    if !output.status.success() {
        bail!("Failed to extract zls: {}", String::from_utf8_lossy(&output.stderr));
    }
    std::fs::remove_dir_all(&temp_dir).ok();

    let bin = install_dir.join("zls");
    if !bin.exists() {
        bail!("zls archive extracted but binary is missing at {}", bin.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn check_zls_version() -> Option<String> {
    let bin = default_install_dir().join("zls");
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["--version"])
}

// ---------------------------------------------------------------------------
// csharp — csharp-ls via `dotnet tool install`, ruby — ruby-lsp via `gem
// install`. Both follow the same isolated-directory pattern as `go install`
// above (an explicit install target directory instead of polluting a
// global tool cache). Both are verified: real outline/definition/doc
// against a live C# project all returned correct results, and outline/
// definition against a live Ruby project did too. ruby-lsp's install step
// (gem install) only places the binary — actually running it composes a
// Bundler-managed bundle on every startup, which needs a working Bundler
// plus a writable gem/bundle path on the host (see CONTRIBUTING.md's Ruby
// setup section for the exact env vars and packages required). Nothing in
// this file can configure that on the user's behalf; it's host Ruby
// environment setup, not something `install_ruby_lsp` below can do.
// ---------------------------------------------------------------------------

fn install_csharp_ls() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Installing csharp-ls via dotnet tool install...");
    let status = Command::new("dotnet").args(["tool", "install", "--tool-path"]).arg(&install_dir).arg("csharp-ls").status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => bail!("dotnet tool install failed with exit code {:?}", s.code()),
        Err(e) => bail!("failed to run dotnet (is the .NET SDK installed and on PATH?): {e}"),
    }
    let bin = install_dir.join("csharp-ls");
    if !bin.exists() {
        bail!("dotnet tool install succeeded but csharp-ls binary is missing at {}", bin.display());
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn check_csharp_ls_version() -> Option<String> {
    let bin = default_install_dir().join("csharp-ls");
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["--version"])
}

fn install_ruby_lsp() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Installing ruby-lsp via gem install...");
    let status = Command::new("gem").args(["install", "ruby-lsp", "--no-document", "--bindir"]).arg(&install_dir).status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => bail!("gem install failed with exit code {:?}", s.code()),
        Err(e) => bail!("failed to run gem (is Ruby installed and on PATH?): {e}"),
    }
    let bin = install_dir.join("ruby-lsp");
    if !bin.exists() {
        bail!("gem install succeeded but ruby-lsp binary is missing at {}", bin.display());
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn check_ruby_lsp_version() -> Option<String> {
    let bin = default_install_dir().join("ruby-lsp");
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["--version"])
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn install_language(language: &str) -> Result<PathBuf> {
    if let Some(spec) = npm_spec(language) {
        return install_npm(&spec);
    }
    match language {
        "go" => install_go(),
        "rust" => install_rust_analyzer().await,
        "java" => install_jdtls().await,
        "kotlin" => install_kotlin().await,
        "cpp" => install_clangd().await,
        "lua" => install_lua_language_server().await,
        "zig" => install_zls().await,
        "csharp" => install_csharp_ls(),
        "ruby" => install_ruby_lsp(),
        other => bail!("Unknown language: {other}\nSupported: {}", MANAGED_LANGUAGES.join(", ")),
    }
}

fn check_version(language: &str) -> Option<String> {
    if let Some(spec) = npm_spec(language) {
        return check_npm_version(&spec);
    }
    match language {
        "go" => check_go_version(),
        "rust" => check_rust_analyzer_version(),
        "java" => check_jdtls_version(),
        "kotlin" => check_kotlin_version(),
        "cpp" => check_clangd_version(),
        "lua" => check_lua_language_server_version(),
        "zig" => check_zls_version(),
        "csharp" => check_csharp_ls_version(),
        "ruby" => check_ruby_lsp_version(),
        _ => None,
    }
}

pub async fn run_install(language: &str, update: bool) -> Result<()> {
    if language == "all" {
        println!("Installing all supported language servers...");
        let mut had_failure = false;
        for lang in MANAGED_LANGUAGES {
            println!("\n--- {lang} ---");
            if let Err(e) = Box::pin(run_install(lang, update)).await {
                eprintln!("\nFailed to install {lang}: {e}");
                had_failure = true;
            }
        }
        if had_failure {
            bail!("One or more language servers failed to install.");
        }
        return Ok(());
    }

    if !is_managed(language) {
        bail!("Unknown language: {language}\nSupported: {}", MANAGED_LANGUAGES.join(", "));
    }

    if let Some(version) = check_version(language) {
        if !update {
            println!("{language} language server already installed: {version}");
            println!("Use 'lsp install <language> --update' to update.");
            return Ok(());
        }
    }

    install_language(language).await?;
    Ok(())
}

/// Auto-installs a missing language server, quietly, before a navigation
/// command needs it. No-op for unmanaged languages (deno relies on PATH;
/// java's jdtls has no single-binary GitHub release to fetch).
/// deno is never installed by us — it's a large, opinionated, self-updating
/// runtime, not a small LSP add-on — but if it's already on the user's
/// `PATH` we should say so and use it, rather than silently doing nothing
/// and letting the daemon fail later with an opaque "No such file or
/// directory" from trying to spawn `deno lsp`.
fn check_deno_version() -> Option<String> {
    run_binary_version(&PathBuf::from("deno"), &["--version"])
}

pub async fn ensure_installed(language: &str) -> Result<()> {
    if language == "deno" {
        return check_deno_version()
            .map(|_| ())
            .ok_or_else(|| anyhow!("deno is not on PATH. Install it from https://deno.land and retry — lsp-cli does not auto-install deno."));
    }
    if !is_managed(language) {
        return Ok(());
    }
    if check_version(language).is_some() {
        return Ok(());
    }
    println!("[lsp] Auto-installing missing language server for {language}...");
    install_language(language).await?;
    Ok(())
}

pub fn run_install_list() -> Result<()> {
    let install_dir = default_install_dir();
    println!("Language servers in: {}\n", install_dir.display());
    println!("{:<14}{:<12}Version", "Language", "Status");
    println!("{}", "\u{2500}".repeat(50));

    for lang in crate::registry::languages() {
        if lang.name == "deno" {
            let version = check_deno_version();
            let status = if version.is_some() { "on PATH" } else { "not found (unmanaged)" };
            println!("{:<14}{:<12}{}", lang.name, status, version.unwrap_or_default());
            continue;
        }
        if !is_managed(lang.name) {
            println!("{:<14}{:<12}", lang.name, "not supported");
            continue;
        }
        let version = check_version(lang.name);
        let status = if version.is_some() { "installed" } else { "missing" };
        println!("{:<14}{:<12}{}", lang.name, status, version.unwrap_or_default());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_analyzer_target_covers_every_documented_platform() {
        assert_eq!(rust_analyzer_target_for("linux", "x86_64").unwrap(), ("x86_64-unknown-linux-gnu", ".gz"));
        assert_eq!(rust_analyzer_target_for("linux", "aarch64").unwrap(), ("aarch64-unknown-linux-gnu", ".gz"));
        assert_eq!(rust_analyzer_target_for("macos", "x86_64").unwrap(), ("x86_64-apple-darwin", ".gz"));
        assert_eq!(rust_analyzer_target_for("macos", "aarch64").unwrap(), ("aarch64-apple-darwin", ".gz"));
        assert_eq!(rust_analyzer_target_for("windows", "x86_64").unwrap(), ("x86_64-pc-windows-msvc", ".zip"));
        assert_eq!(rust_analyzer_target_for("windows", "aarch64").unwrap(), ("aarch64-pc-windows-msvc", ".zip"));
    }

    #[test]
    fn rust_analyzer_target_errors_cleanly_on_unsupported_platform() {
        let err = rust_analyzer_target_for("freebsd", "riscv64").unwrap_err();
        assert!(err.to_string().contains("Unsupported OS/arch"));
    }

    #[test]
    fn windows_targets_use_zip_others_use_gz() {
        let (_, win_ext) = rust_analyzer_target_for("windows", "x86_64").unwrap();
        let (_, linux_ext) = rust_analyzer_target_for("linux", "x86_64").unwrap();
        let (_, mac_ext) = rust_analyzer_target_for("macos", "aarch64").unwrap();
        assert_eq!(win_ext, ".zip");
        assert_eq!(linux_ext, ".gz");
        assert_eq!(mac_ext, ".gz");
    }

    #[test]
    fn write_node_wrapper_produces_a_correct_exec_script() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("some-server");
        let entry = dir.path().join("node_modules/some-server/bin/main.js");

        write_node_wrapper(&wrapper, &entry).unwrap();

        let contents = std::fs::read_to_string(&wrapper).unwrap();
        assert!(contents.starts_with("#!/bin/sh\n"));
        assert!(contents.contains(&format!("exec node \"{}\" \"$@\"", entry.display())));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&wrapper).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "wrapper should be executable for user/group/other");
        }
    }

    #[test]
    fn managed_languages_all_have_an_install_path() {
        // Every entry in MANAGED_LANGUAGES must resolve to either an npm
        // spec or one of the explicit go/rust/kotlin arms in
        // install_language/check_version — otherwise `lsp install <lang>`
        // would claim to support a language it can't actually install.
        for lang in MANAGED_LANGUAGES {
            let has_npm_spec = npm_spec(lang).is_some();
            let has_explicit_arm = matches!(*lang, "go" | "rust" | "java" | "kotlin" | "cpp" | "lua" | "zig" | "csharp" | "ruby");
            assert!(has_npm_spec || has_explicit_arm, "managed language `{lang}` has no install path wired up");
        }
    }

    #[test]
    fn npm_spec_wrapper_names_are_unique_per_language() {
        // Two languages accidentally sharing a wrapper_name would silently
        // clobber each other's installed server on disk.
        let mut seen = std::collections::HashSet::new();
        for lang in MANAGED_LANGUAGES {
            if let Some(spec) = npm_spec(lang) {
                assert!(seen.insert(spec.wrapper_name), "duplicate wrapper_name `{}` for language `{lang}`", spec.wrapper_name);
            }
        }
    }

    #[test]
    fn kotlin_server_bin_uses_platform_appropriate_extension() {
        let dir = tempfile::tempdir().unwrap();
        let bin = kotlin_server_bin(dir.path());
        if std::env::consts::OS == "windows" {
            assert!(bin.to_string_lossy().ends_with(".bat"));
        } else {
            assert!(!bin.to_string_lossy().ends_with(".bat"));
        }
        assert!(bin.starts_with(dir.path()));
    }
}
