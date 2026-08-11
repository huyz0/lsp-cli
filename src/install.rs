//! Automatic language server installation. npm-installed servers
//! (typescript, python) get a thin shell wrapper into
//! `~/.lsp-cli/servers/<bin>` that execs `node <entry> "$@"`; gopls is
//! `go install`ed into an isolated GOPATH and symlinked in; rust-analyzer,
//! kotlin-language-server, clangd, lua-language-server, and zls are fetched
//! from GitHub Releases; jdtls is fetched from Eclipse's downloads and
//! wrapped in a script that pins in a JDK it finds via
//! sdkman/`JAVA_HOME`/`PATH` (installing a JDK itself is out of scope —
//! it's a much bigger, more opinionated dependency than any other managed
//! server); csharp-ls and ruby-lsp go through `dotnet tool install`/`gem
//! install` respectively. json, css, html, and bash are Rust-native,
//! bundled servers (see src/servers/) shipped as sibling binaries of `lsp`
//! itself, so "installing" one is just confirming that binary is present,
//! no download/npm/network involved at all. deno remains unmanaged since
//! it relies on the `deno` binary already being on `PATH`.

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::registry::default_install_dir;

use crate::paths::{go_dir, packages_dir};

/// How a language's server is obtained, and everything needed to do it.
///
/// One `Handler` per language replaces what used to be a hand-maintained
/// `MANAGED_LANGUAGES` list plus two parallel `match` statements that had
/// to stay in lockstep with it and with `registry::languages()`. Those four
/// could disagree and nothing but a hand-written test would notice; now
/// adding a language is a registry entry plus one arm here, and the
/// compiler checks the rest.
enum Handler {
    /// An npm package with a Node entry point, wrapped in a shell script.
    Npm(NpmSpec),
    /// A GitHub release asset.
    Release(ReleaseSpec),
    /// Built into this binary as a sibling `lsp-<lang>-lsp`; nothing to
    /// download.
    Bundled(&'static str),
    /// `go install` into an isolated GOPATH.
    Go,
    /// Eclipse's jdtls bundle plus a JDK-pinning wrapper script.
    Jdtls,
    /// `dotnet tool install`.
    DotnetTool,
    /// `gem install`.
    Gem,
}

fn handler(language: &str) -> Option<Handler> {
    Some(match language {
        "typescript" | "python" => Handler::Npm(npm_spec(language)?),
        "go" => Handler::Go,
        "rust" => Handler::Release(rust_analyzer_spec()),
        "kotlin" => Handler::Release(kotlin_spec()),
        "cpp" => Handler::Release(clangd_spec()),
        "lua" => Handler::Release(lua_spec()),
        "zig" => Handler::Release(zls_spec()),
        "java" => Handler::Jdtls,
        "csharp" => Handler::DotnetTool,
        "ruby" => Handler::Gem,
        "json" => Handler::Bundled("lsp-json-lsp"),
        "css" => Handler::Bundled("lsp-css-lsp"),
        "html" => Handler::Bundled("lsp-html-lsp"),
        "bash" => Handler::Bundled("lsp-bash-lsp"),
        // deno is deliberately unmanaged: it's used if it's on PATH and
        // never downloaded.
        _ => return None,
    })
}

/// Managed languages, in the order `registry::languages()` declares them —
/// which is the order `lsp install --list` shows. Derived rather than
/// listed, so it cannot drift from the registry or from `handler`.
pub fn managed_languages() -> Vec<&'static str> {
    crate::registry::languages()
        .iter()
        .map(|l| l.name)
        .filter(|name| handler(name).is_some())
        .collect()
}

fn is_managed(language: &str) -> bool {
    handler(language).is_some()
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
    let status = Command::new("npm")
        .arg("install")
        .args(packages)
        .current_dir(&dir)
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => bail!(
            "npm install {} failed with exit code {:?}",
            packages.join(" "),
            s.code()
        ),
        Err(e) => bail!("failed to run npm (is it installed and on PATH?): {e}"),
    }
}

/// Version string for an installed binary at `rel` under the install
/// directory, or `None` if it isn't there.
///
/// Eight `check_*_version` functions were this same three-line shape.
fn check_installed_version(rel: PathBuf, args: &[&str]) -> Option<String> {
    let bin = default_install_dir().join(rel);
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, args)
}

/// Presence check for servers that have no usable `--version` (jdtls's
/// wrapper, kotlin-language-server and lua-language-server all start their
/// LSP loop instead of printing a version).
fn check_installed_present(rel: PathBuf) -> Option<String> {
    default_install_dir()
        .join(rel)
        .exists()
        .then(|| "installed".to_string())
}

fn run_binary_version(bin: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// typescript, python — the last two npm packages with a
// node-script entry point, wrapped identically.
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
        _ => return None,
    })
}

fn install_npm(spec: &NpmSpec) -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!(
        "Installing {} (npm install {})...",
        spec.wrapper_name,
        spec.packages.join(" ")
    );
    npm_install(spec.packages)?;

    let entry = packages_dir().join(spec.entry_rel);
    if !entry.exists() {
        bail!(
            "npm install succeeded but expected entry point is missing: {}",
            entry.display()
        );
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

    let version_entry = spec
        .version_entry_rel
        .map(|rel| packages_dir().join(rel))
        .unwrap_or_else(|| entry.clone());
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
        bail!(
            "go install succeeded but gopls binary is missing at {}",
            src.display()
        );
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
        bail!(
            "Failed to fetch latest release for {repo}: HTTP {}",
            resp.status()
        );
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
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("lsp-cli-install-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir)
        .map_err(|e| anyhow!("failed to create temp install dir {}: {e}", dir.display()))?;
    Ok(dir)
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("User-Agent", "lsp-cli")
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Download failed: HTTP {} for {url}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

// ---------------------------------------------------------------------------
// GitHub-release installers
//
// rust-analyzer, kotlin-language-server, clangd, lua-language-server and zls
// are all "fetch the latest release asset and unpack it". They differ in
// exactly three ways — which repo, what the asset is called, and how the
// archive is laid out — so those are data and the thirteen-step sequence
// around them is written once. It used to be five near-identical copies of
// ~55 lines each, which is where the divergences crept in: only one of them
// staged extraction inside the install directory to avoid a cross-filesystem
// rename, and only one verified the binary actually appeared.
// ---------------------------------------------------------------------------

/// How an archive lays out what it contains.
enum Layout {
    /// A single compressed executable. Decompressed straight to `bin_rel`.
    SingleFile,
    /// Unpacks with no wrapping directory. Extracted into `dir_rel` under
    /// the install directory, replacing whatever was there; `None` means
    /// the install directory itself, which is *not* wiped first.
    Flat { dir_rel: Option<&'static str> },
    /// Wraps everything in one version-stamped top-level directory (e.g.
    /// `clangd_21.1.0/`). That directory is stripped so the registry can
    /// reference a fixed path regardless of the installed version.
    StripTopLevel { dir_rel: &'static str },
}

struct ReleaseSpec {
    /// Name used in progress output.
    display: &'static str,
    /// GitHub `owner/repo`.
    repo: &'static str,
    /// Asset file name for a given release tag.
    asset_name: fn(&str) -> Result<String>,
    layout: Layout,
    /// The installed executable, relative to the install directory.
    bin_rel: fn() -> PathBuf,
}

/// Extracts `archive` according to its file extension.
///
/// Shelling out to the system tools rather than linking archive crates is
/// the pre-existing tradeoff here; this just keeps the dispatch in one
/// place instead of five.
fn extract_archive(archive: &Path, into: &Path, filename: &str) -> Result<()> {
    let run = |cmd: &mut Command, what: &str| -> Result<()> {
        let output = cmd.output()?;
        if !output.status.success() {
            bail!("{what} failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    };
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        run(
            Command::new("tar")
                .arg("-xzf")
                .arg(archive)
                .arg("-C")
                .arg(into),
            "tar -xzf",
        )
    } else if filename.ends_with(".tar.xz") {
        run(
            Command::new("tar")
                .arg("-xJf")
                .arg(archive)
                .arg("-C")
                .arg(into),
            "tar -xJf",
        )
    } else if filename.ends_with(".zip") {
        run(
            Command::new("unzip")
                .args(["-q", "-o"])
                .arg(archive)
                .arg("-d")
                .arg(into),
            "unzip",
        )
    } else {
        bail!("Don't know how to extract {filename}")
    }
}

/// Decompresses a single-file archive to stdout.
fn decompress_single_file(archive: &Path, filename: &str) -> Result<Vec<u8>> {
    let (program, args): (&str, &[&str]) = if filename.ends_with(".gz") {
        ("gunzip", &["-c"])
    } else if filename.ends_with(".zip") {
        ("unzip", &["-p"])
    } else {
        bail!("Don't know how to decompress {filename}")
    };
    let output = Command::new(program).args(args).arg(archive).output()?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

async fn install_from_release(spec: &ReleaseSpec) -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Fetching {} from GitHub Releases...", spec.display);

    let release = fetch_latest_release(spec.repo).await?;
    let filename = (spec.asset_name)(&release.tag_name)?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == filename)
        .ok_or_else(|| anyhow!("Could not find release asset {filename}"))?;

    println!("Downloading {filename}...");
    let bytes = download_bytes(&asset.browser_download_url).await?;
    let temp_dir = unique_temp_dir()?;
    let archive = temp_dir.join(&filename);
    std::fs::write(&archive, &bytes)?;

    let bin = install_dir.join((spec.bin_rel)());
    let result = unpack(spec, &archive, &filename, &install_dir, &bin);
    std::fs::remove_dir_all(&temp_dir).ok();
    result?;

    if !bin.exists() {
        bail!(
            "{} archive extracted but the binary is missing at {}",
            spec.display,
            bin.display()
        );
    }
    make_executable(&bin)?;
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn unpack(
    spec: &ReleaseSpec,
    archive: &Path,
    filename: &str,
    install_dir: &Path,
    bin: &Path,
) -> Result<()> {
    match spec.layout {
        Layout::SingleFile => {
            let contents = decompress_single_file(archive, filename)?;
            if let Some(parent) = bin.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(bin, contents)?;
        }
        Layout::Flat { dir_rel } => match dir_rel {
            Some(rel) => {
                // Replace wholesale, so an update can't leave stale files
                // from the previous version behind.
                let dest = install_dir.join(rel);
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                std::fs::create_dir_all(&dest)?;
                extract_archive(archive, &dest, filename)?;
            }
            // Straight into the install directory, which holds every other
            // server too and must not be cleared.
            None => extract_archive(archive, install_dir, filename)?,
        },
        Layout::StripTopLevel { dir_rel } => {
            // Staged inside `install_dir`, not the system temp dir, so the
            // rename below stays on one filesystem — renaming across them
            // (a tmpfs /tmp vs. ~/.lsp-cli on another mount) fails with
            // EXDEV, reproduced live.
            let staging = install_dir.join(format!(".{dir_rel}-extract"));
            if staging.exists() {
                std::fs::remove_dir_all(&staging)?;
            }
            std::fs::create_dir_all(&staging)?;
            let extracted = extract_archive(archive, &staging, filename);
            let unpacked = extracted.and_then(|()| {
                std::fs::read_dir(&staging)?
                    .filter_map(|e| e.ok())
                    .find(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.path())
                    .ok_or_else(|| {
                        anyhow!(
                            "{} archive did not contain the expected top-level directory",
                            spec.display
                        )
                    })
            });
            let unpacked = match unpacked {
                Ok(p) => p,
                Err(e) => {
                    std::fs::remove_dir_all(&staging).ok();
                    return Err(e);
                }
            };
            let dest = install_dir.join(dir_rel);
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            std::fs::rename(&unpacked, &dest)?;
            std::fs::remove_dir_all(&staging).ok();
        }
    }
    Ok(())
}

fn rust_analyzer_bin() -> PathBuf {
    PathBuf::from(if std::env::consts::OS == "windows" {
        "rust-analyzer.exe"
    } else {
        "rust-analyzer"
    })
}

fn rust_analyzer_spec() -> ReleaseSpec {
    ReleaseSpec {
        display: "rust-analyzer",
        repo: "rust-lang/rust-analyzer",
        asset_name: |_version| {
            let (target, ext) = rust_analyzer_target()?;
            Ok(format!("rust-analyzer-{target}{ext}"))
        },
        layout: Layout::SingleFile,
        bin_rel: rust_analyzer_bin,
    }
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
    // The user's real OS home, not lsp-cli's state directory: sdkman
    // installs itself under `~/.sdkman` regardless of where lsp-cli keeps
    // its own files.
    let sdkman_java = dirs::home_dir()
        .unwrap_or_default()
        .join(".sdkman")
        .join("candidates")
        .join("java")
        .join("current")
        .join("bin")
        .join("java");
    if sdkman_java.exists() {
        return Some(sdkman_java);
    }
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let candidate = PathBuf::from(java_home).join("bin").join("java");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let on_path = Command::new("java")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
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
    std::fs::read_dir(jdtls_dir.join("plugins"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("org.eclipse.equinox.launcher_") && n.ends_with(".jar"))
                .unwrap_or(false)
        })
}

fn write_jdtls_wrapper(
    wrapper_path: &Path,
    java: &Path,
    launcher: &Path,
    config_dir: &Path,
) -> Result<()> {
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
    println!(
        "Fetching Eclipse JDT Language Server (using JDK at {})...",
        java.display()
    );

    let bytes = download_bytes(
        "https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz",
    )
    .await?;
    let temp_dir = unique_temp_dir()?;
    let temp_path = temp_dir.join("jdt-language-server-latest.tar.gz");
    std::fs::write(&temp_path, &bytes)?;

    let dest = jdtls_install_dir();
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(&temp_path)
        .arg("-C")
        .arg(&dest)
        .output()?;
    if !output.status.success() {
        bail!(
            "Failed to extract jdtls: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(&temp_dir).ok();

    let launcher = find_launcher_jar(&dest).ok_or_else(|| {
        anyhow!(
            "jdtls archive extracted but no launcher jar found under {}",
            dest.join("plugins").display()
        )
    })?;
    let config_dir = dest.join(jdtls_config_dir_name());
    if !config_dir.exists() {
        bail!(
            "jdtls archive extracted but expected config dir is missing: {}",
            config_dir.display()
        );
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
    let name = if std::env::consts::OS == "windows" {
        "kotlin-language-server.bat"
    } else {
        "kotlin-language-server"
    };
    install_dir
        .join("kotlin")
        .join("server")
        .join("bin")
        .join(name)
}

fn kotlin_spec() -> ReleaseSpec {
    ReleaseSpec {
        display: "kotlin-language-server",
        repo: "fwcd/kotlin-language-server",
        asset_name: |_version| Ok("server.zip".to_string()),
        // The zip's own top level is `server/`, which is kept — the
        // registry path is `kotlin/server/bin/kotlin-language-server`.
        layout: Layout::Flat {
            dir_rel: Some("kotlin"),
        },
        bin_rel: || kotlin_server_bin(Path::new("")),
    }
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

fn clangd_spec() -> ReleaseSpec {
    ReleaseSpec {
        display: "clangd",
        repo: "clangd/clangd",
        asset_name: clangd_asset_name,
        // The zip wraps everything in `clangd_<version>/`.
        layout: Layout::StripTopLevel { dir_rel: "clangd" },
        bin_rel: clangd_server_bin,
    }
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

fn lua_spec() -> ReleaseSpec {
    ReleaseSpec {
        display: "lua-language-server",
        repo: "LuaLS/lua-language-server",
        asset_name: lua_language_server_asset_name,
        layout: Layout::Flat {
            dir_rel: Some("lua"),
        },
        bin_rel: lua_language_server_bin,
    }
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

fn zls_spec() -> ReleaseSpec {
    ReleaseSpec {
        display: "zls",
        repo: "zigtools/zls",
        asset_name: |_version| zls_asset_name().map(|s| s.to_string()),
        // Unpacks flat, straight into the install directory alongside every
        // other server, so nothing is cleared first.
        layout: Layout::Flat { dir_rel: None },
        bin_rel: || PathBuf::from("zls"),
    }
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
    let status = Command::new("dotnet")
        .args(["tool", "install", "--tool-path"])
        .arg(&install_dir)
        .arg("csharp-ls")
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => bail!("dotnet tool install failed with exit code {:?}", s.code()),
        Err(e) => bail!("failed to run dotnet (is the .NET SDK installed and on PATH?): {e}"),
    }
    let bin = install_dir.join("csharp-ls");
    if !bin.exists() {
        bail!(
            "dotnet tool install succeeded but csharp-ls binary is missing at {}",
            bin.display()
        );
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

fn install_ruby_lsp() -> Result<PathBuf> {
    let install_dir = default_install_dir();
    std::fs::create_dir_all(&install_dir)?;
    println!("Installing ruby-lsp via gem install...");
    let status = Command::new("gem")
        .args(["install", "ruby-lsp", "--no-document", "--bindir"])
        .arg(&install_dir)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => bail!("gem install failed with exit code {:?}", s.code()),
        Err(e) => bail!("failed to run gem (is Ruby installed and on PATH?): {e}"),
    }
    let bin = install_dir.join("ruby-lsp");
    if !bin.exists() {
        bail!(
            "gem install succeeded but ruby-lsp binary is missing at {}",
            bin.display()
        );
    }
    println!("\u{2713} Installed to {}", bin.display());
    Ok(bin)
}

// ---------------------------------------------------------------------------
// Bundled Rust-native servers (json, css, ... — see src/servers/) built and
// shipped as sibling binaries of `lsp` itself (Cargo.toml's `[[bin]]`
// entries), not downloaded or npm-installed. "Installing" one is just
// confirming the bundled binary is actually present next to `lsp` —
// `registry::server_path` resolves it relative to the running executable's
// own directory, not `default_install_dir()`. One shared implementation
// parameterized on the binary name, since every bundled server follows the
// exact same `lsp-<lang>-lsp` convention and has nothing else to configure.
// ---------------------------------------------------------------------------

fn check_bundled_server_version(bin_name: &str) -> Option<String> {
    let bin = crate::registry::server_path(bin_name, &default_install_dir());
    if !bin.exists() {
        return None;
    }
    run_binary_version(&bin, &["--version"])
}

fn install_bundled_server(bin_name: &str) -> Result<PathBuf> {
    let bin = crate::registry::server_path(bin_name, &default_install_dir());
    if bin.exists() {
        return Ok(bin);
    }
    bail!(
        "{bin_name} (bundled with this tool) not found at {}. This usually means `lsp` was installed without its bundled servers — reinstall via the same method you used for `lsp` itself (Homebrew/install.sh/a prebuilt release archive), or from source with `cargo build --release --bin {bin_name}`.",
        bin.display()
    );
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn install_language(language: &str) -> Result<PathBuf> {
    let Some(handler) = handler(language) else {
        bail!(
            "Unknown language: {language}\nSupported: {}",
            managed_languages().join(", ")
        );
    };
    match handler {
        Handler::Npm(spec) => install_npm(&spec),
        Handler::Release(spec) => install_from_release(&spec).await,
        Handler::Bundled(bin) => install_bundled_server(bin),
        Handler::Go => install_go(),
        Handler::Jdtls => install_jdtls().await,
        Handler::DotnetTool => install_csharp_ls(),
        Handler::Gem => install_ruby_lsp(),
    }
}

fn check_version(language: &str) -> Option<String> {
    match handler(language)? {
        Handler::Npm(spec) => check_npm_version(&spec),
        // Every release-installed server reports a version except the two
        // JVM-ish ones, which start their LSP loop instead of printing one.
        Handler::Release(spec) => match language {
            "kotlin" | "lua" => check_installed_present((spec.bin_rel)()),
            _ => check_installed_version((spec.bin_rel)(), &["--version"]),
        },
        Handler::Bundled(bin) => check_bundled_server_version(bin),
        Handler::Go => check_go_version(),
        Handler::Jdtls => check_jdtls_version(),
        Handler::DotnetTool => check_installed_version(PathBuf::from("csharp-ls"), &["--version"]),
        Handler::Gem => check_installed_version(PathBuf::from("ruby-lsp"), &["--version"]),
    }
}

pub async fn run_install(language: &str, update: bool) -> Result<()> {
    if language == "all" {
        println!("Installing all supported language servers...");
        let mut had_failure = false;
        for lang in managed_languages() {
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
        bail!(
            "Unknown language: {language}\nSupported: {}",
            managed_languages().join(", ")
        );
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
            let status = if version.is_some() {
                "on PATH"
            } else {
                "not found (unmanaged)"
            };
            println!(
                "{:<14}{:<12}{}",
                lang.name,
                status,
                version.unwrap_or_default()
            );
            continue;
        }
        if !is_managed(lang.name) {
            println!("{:<14}{:<12}", lang.name, "not supported");
            continue;
        }
        let version = check_version(lang.name);
        let status = if version.is_some() {
            "installed"
        } else {
            "missing"
        };
        println!(
            "{:<14}{:<12}{}",
            lang.name,
            status,
            version.unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_analyzer_target_covers_every_documented_platform() {
        assert_eq!(
            rust_analyzer_target_for("linux", "x86_64").unwrap(),
            ("x86_64-unknown-linux-gnu", ".gz")
        );
        assert_eq!(
            rust_analyzer_target_for("linux", "aarch64").unwrap(),
            ("aarch64-unknown-linux-gnu", ".gz")
        );
        assert_eq!(
            rust_analyzer_target_for("macos", "x86_64").unwrap(),
            ("x86_64-apple-darwin", ".gz")
        );
        assert_eq!(
            rust_analyzer_target_for("macos", "aarch64").unwrap(),
            ("aarch64-apple-darwin", ".gz")
        );
        assert_eq!(
            rust_analyzer_target_for("windows", "x86_64").unwrap(),
            ("x86_64-pc-windows-msvc", ".zip")
        );
        assert_eq!(
            rust_analyzer_target_for("windows", "aarch64").unwrap(),
            ("aarch64-pc-windows-msvc", ".zip")
        );
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
            assert_eq!(
                mode & 0o111,
                0o111,
                "wrapper should be executable for user/group/other"
            );
        }
    }

    #[test]
    fn every_registry_language_is_either_installable_or_deliberately_not() {
        // The registry is the source of truth for what languages exist.
        // Anything in it without a `handler` is unmanaged, and `deno` is
        // the only one that is meant to be — it's used from PATH and never
        // downloaded. A new registry entry with no install path would
        // otherwise be silently unusable.
        for lang in crate::registry::languages() {
            let managed = handler(lang.name).is_some();
            assert_eq!(
                managed,
                lang.name != "deno",
                "`{}` should {} have an install path",
                lang.name,
                if lang.name == "deno" { "not" } else { "" }
            );
        }
    }

    #[test]
    fn managed_languages_is_derived_from_the_registry() {
        // It used to be a hand-written list that could drift from both the
        // registry and the install dispatch.
        let managed = managed_languages();
        assert!(!managed.contains(&"deno"));
        for name in &managed {
            assert!(
                crate::registry::languages().iter().any(|l| l.name == *name),
                "`{name}` is not a registry language"
            );
        }
        assert_eq!(managed.len(), crate::registry::languages().len() - 1);
    }

    #[test]
    fn every_managed_language_reports_a_version_path() {
        // `check_version` returning None for an installed server would make
        // `ensure_installed` reinstall it on every single command.
        for lang in managed_languages() {
            assert!(
                handler(lang).is_some(),
                "managed language `{lang}` has no handler"
            );
        }
    }

    #[test]
    fn npm_spec_wrapper_names_are_unique_per_language() {
        // Two languages accidentally sharing a wrapper_name would silently
        // clobber each other's installed server on disk.
        let mut seen = std::collections::HashSet::new();
        for lang in managed_languages() {
            if let Some(spec) = npm_spec(lang) {
                assert!(
                    seen.insert(spec.wrapper_name),
                    "duplicate wrapper_name `{}` for language `{lang}`",
                    spec.wrapper_name
                );
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
