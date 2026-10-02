mod support;
use support::lsp;

#[test]
fn install_list_shows_all_managed_languages() {
    let result = lsp(&["install", "--list"]);
    assert_eq!(result.exit_code, 0);
    for lang in [
        "typescript",
        "python",
        "go",
        "rust",
        "java",
        "kotlin",
        "html",
        "css",
        "json",
        "cpp",
        "lua",
        "zig",
        "bash",
        "csharp",
        "ruby",
    ] {
        assert!(
            result.stdout.contains(lang),
            "expected {lang} in install --list output:\n{}",
            result.stdout
        );
    }
    // deno relies on PATH rather than being auto-installed, but should
    // still be listed with its detected status.
    assert!(result.stdout.contains("deno"));
}

#[test]
fn install_unknown_language_errors() {
    let result = lsp(&["install", "not-a-real-language"]);
    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.contains("Unknown language"));
}

/// `defaultMaxItems` and `managerTimeout` were parsed from
/// `~/.lsp-cli/config.json`, documented in the README, unit tested — and
/// then never read by anything. These check the wiring end to end, through
/// the real config file the CLI loads.
#[test]
fn default_max_items_from_config_is_actually_applied() {
    let home = support::isolated_home("config-max-items");
    std::fs::write(home.path().join("config.json"), r#"{"defaultMaxItems": 1}"#).unwrap();

    // `lsp schema reference` reports the documented default; the value that
    // matters is the one `--max-items` falls back to, which is observable
    // through search's own JSON (it reports `total` and `startIndex`).
    let result = support::lsp_in(&home, &["search", "e", "--output", "json"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let items = data["items"].as_array().unwrap();
    assert!(
        items.len() <= 1,
        "defaultMaxItems=1 should cap the page at one item, got {}",
        items.len()
    );
}

#[test]
fn a_malformed_config_file_still_leaves_the_cli_usable() {
    let home = support::isolated_home("config-malformed");
    std::fs::write(home.path().join("config.json"), "{ not json").unwrap();
    let result = support::lsp_in(&home, &["install", "--list"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
}

/// A `PATH` with nothing but `fakebin` and the system basics (`sh`,
/// `sleep`, `mkdir`, ...), so no real language server is reachable.
fn fake_path(fakebin: &std::path::Path) -> String {
    format!("{}:/usr/bin:/bin", fakebin.display())
}

/// Auto-install runs in the middle of a navigation command whose stdout is
/// a JSON document (and, under `lsp mcp`, a tool result). Both our own
/// progress lines and the package manager's chatter used to land there.
#[test]
fn auto_install_output_never_reaches_stdout() {
    let home = support::empty_home("stdout-purity");
    let fakebin = home.path().join("fakebin");
    std::fs::create_dir_all(&fakebin).unwrap();
    support::write_script(
        &fakebin,
        "npm",
        "echo 'added 1 package in 1s'\necho 'npm went wrong' >&2\nexit 1",
    );
    let file = format!(
        "{}/tests/fixtures/typescript_project/src/models.ts",
        env!("CARGO_MANIFEST_DIR")
    );
    let path = fake_path(&fakebin);
    let result = support::lsp_in_env(
        &home,
        &["outline", &file, "--output", "json"],
        &[("PATH", &path)],
    );
    assert_ne!(
        result.exit_code, 0,
        "the fake npm fails, so must the command"
    );
    assert_eq!(
        result.stdout, "",
        "stdout must carry nothing but the result"
    );
    assert!(
        result.stderr.contains("added 1 package"),
        "npm's own output belongs on stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Auto-installing"),
        "{}",
        result.stderr
    );
}

/// Several first-use commands at once used to run `npm install` into the
/// same directory concurrently and break each other. The fake npm records
/// whether it ever found another install still in progress.
#[test]
fn concurrent_installs_of_one_language_never_overlap() {
    let home = support::empty_home("install-lock");
    let fakebin = home.path().join("fakebin");
    let probe = home.path().join("probe");
    std::fs::create_dir_all(&fakebin).unwrap();
    std::fs::create_dir_all(&probe).unwrap();
    support::write_script(
        &fakebin,
        "npm",
        &format!(
            r#"P="{}"
if [ -e "$P/busy" ]; then touch "$P/overlap"; fi
touch "$P/busy"
sleep 1
mkdir -p node_modules/typescript-language-server/lib
echo '// fake' > node_modules/typescript-language-server/lib/cli.mjs
rm -f "$P/busy"
echo "added 1 package""#,
            probe.display()
        ),
    );
    let path = fake_path(&fakebin);
    let results: Vec<support::RunResult> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..3)
            .map(|_| {
                s.spawn(|| {
                    support::lsp_in_env(&home, &["install", "typescript"], &[("PATH", &path)])
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for r in &results {
        assert_eq!(r.exit_code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
        assert!(!r.stdout.contains("added 1 package"), "{}", r.stdout);
    }
    assert!(
        !probe.join("overlap").exists(),
        "two npm installs ran at the same time"
    );
    let wrapper = home
        .path()
        .join("servers")
        .join("typescript-language-server");
    let script = std::fs::read_to_string(&wrapper).unwrap();
    assert!(
        script.contains("typescript-language-server/lib/cli.mjs"),
        "{script}"
    );
}

/// rustup puts a `rust-analyzer` proxy on `PATH` that exits non-zero when
/// the component isn't installed; a server that can't answer its version
/// probe must not count as available.
#[test]
fn install_list_reports_a_working_server_on_path_and_ignores_a_broken_one() {
    let home = support::empty_home("path-list");
    let fakebin = home.path().join("fakebin");
    std::fs::create_dir_all(&fakebin).unwrap();
    support::write_script(&fakebin, "zls", "echo 0.13.0");
    support::write_script(
        &fakebin,
        "rust-analyzer",
        "echo \"error: Unknown binary 'rust-analyzer'\" >&2\nexit 1",
    );
    let path = fake_path(&fakebin);
    let result = support::lsp_in_env(&home, &["install", "--list"], &[("PATH", &path)]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let line = |lang: &str| {
        result
            .stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(lang))
            .unwrap_or_else(|| panic!("no {lang} row in:\n{}", result.stdout))
            .to_string()
    };
    let zig = line("zig");
    assert!(zig.contains("on PATH") && zig.contains("0.13.0"), "{zig}");
    assert!(
        zig.contains(&fakebin.join("zls").display().to_string()),
        "{zig}"
    );
    assert!(line("rust").contains("missing"), "{}", line("rust"));

    // And `lsp install` uses it rather than downloading.
    let result = support::lsp_in_env(&home, &["install", "zig"], &[("PATH", &path)]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert!(result.stdout.contains("found on PATH"), "{}", result.stdout);
    assert!(!home.path().join("servers").join("zls").exists());

    // Unless the config opts out.
    std::fs::write(
        home.path().join("config.json"),
        r#"{"usePathServers": false}"#,
    )
    .unwrap();
    let result = support::lsp_in_env(&home, &["install", "--list"], &[("PATH", &path)]);
    let zig = result
        .stdout
        .lines()
        .find(|l| l.starts_with("zig"))
        .unwrap()
        .to_string();
    assert!(zig.contains("missing"), "{zig}");
}

/// End to end: with nothing installed, a real gopls on `PATH` serves a
/// navigation command and nothing is downloaded.
#[test]
fn a_real_server_on_path_serves_navigation_without_installing() {
    let Some(gopls) = [
        support::which("gopls"),
        dirs::home_dir().map(|h| h.join("go/bin/gopls")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.exists()) else {
        eprintln!("skipping: gopls not found");
        return;
    };
    let Some(go) = support::which("go") else {
        eprintln!("skipping: go not on PATH");
        return;
    };
    let home = support::empty_home("path-gopls");
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        gopls.parent().unwrap().display(),
        go.parent().unwrap().display()
    );
    let file = format!(
        "{}/tests/fixtures/go_project/models.go",
        env!("CARGO_MANIFEST_DIR")
    );
    let result = support::lsp_in_env(
        &home,
        &["outline", &file, "--output", "json"],
        &[("PATH", &path)],
    );
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let symbols: serde_json::Value =
        serde_json::from_str(&result.stdout).unwrap_or_else(|e| panic!("{e}: {}", result.stdout));
    let names: Vec<&str> = symbols["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items: {symbols}"))
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(names.contains(&"User"), "{names:?}");
    assert!(!home.path().join("servers").join("gopls").exists());
    assert!(
        !result.stderr.contains("Auto-installing"),
        "{}",
        result.stderr
    );
}

/// The daemon is long-lived and keeps the environment of whatever started
/// it — e.g. an editor launching `lsp mcp` with a minimal `PATH`. A server
/// the CLI found on *its* `PATH` must still be the one that runs, so the
/// CLI passes the resolved binary to the daemon instead of letting the
/// daemon look again and come up empty.
#[test]
fn the_daemon_runs_the_server_the_cli_resolved_even_with_a_different_path() {
    let project = support::FakeServerProject::new("pinned-bin", "fn only() {}\n", &[]);
    // Start the daemon from an environment where no `zls` exists.
    let started = support::lsp_in_env(
        &project.home,
        &["server", "list"],
        &[("PATH", "/usr/bin:/bin")],
    );
    assert_eq!(started.exit_code, 0, "{}", started.stderr);

    let file = project.file();
    let result = project.run(&["outline", &file, "--output", "json"]);
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(data["items"][0]["name"], "only", "{data}");
}
