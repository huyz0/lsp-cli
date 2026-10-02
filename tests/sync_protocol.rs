//! What the daemon sends a language server, observed from the server's side
//! via the scriptable fake server (`tests/support/fake_lsp.py`), which logs
//! every method it receives. These pin the document-sync rules that make
//! warm commands both fast and correct.

mod support;
use support::FakeServerProject;

fn log_of(project: &FakeServerProject) -> Vec<String> {
    std::fs::read_to_string(project.home.path().join("server.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn count(log: &[String], method: &str) -> usize {
    log.iter().filter(|m| *m == method).count()
}

fn project(label: &str, source: &str) -> FakeServerProject {
    let p = FakeServerProject::new(label, source, &[]);
    p.set_env(
        "FAKE_LSP_LOG",
        &p.home.path().join("server.log").display().to_string(),
    );
    p
}

fn ok(project: &FakeServerProject, args: &[&str]) -> String {
    let r = project.run(args);
    assert_eq!(r.exit_code, 0, "lsp {args:?}: {}", r.stderr);
    r.stdout
}

#[test]
fn an_unchanged_file_is_not_resent_and_an_edited_one_is_sent_once() {
    let p = project("sync-unchanged", "fn a() {}\n");
    let file = p.file();
    ok(&p, &["outline", &file]);
    ok(&p, &["outline", &file]);
    ok(&p, &["outline", &file]);
    let log = log_of(&p);
    assert_eq!(count(&log, "textDocument/didOpen"), 1, "{log:?}");
    assert_eq!(count(&log, "textDocument/didChange"), 0, "{log:?}");

    std::fs::write(&file, "fn a() {}\nfn b() {}\n").unwrap();
    let out = ok(&p, &["outline", &file]);
    assert!(out.contains("\"b\""), "{out}");
    assert_eq!(count(&log_of(&p), "textDocument/didChange"), 1);
}

#[test]
fn an_open_file_edited_on_disk_is_resynced_before_a_query_on_another_file() {
    let p = project("sync-other", "fn main_fn() {}\n");
    let other = p.dir.join("other.zig");
    std::fs::write(&other, "fn helper() {}\n").unwrap();
    let file = p.file();
    let other_s = other.display().to_string();
    ok(&p, &["outline", &other_s]);
    ok(&p, &["outline", &file]);
    assert_eq!(count(&log_of(&p), "textDocument/didChange"), 0);

    std::fs::write(&other, "fn helper() {}\nfn helper2() {}\n").unwrap();
    ok(&p, &["outline", &file]);
    assert_eq!(
        count(&log_of(&p), "textDocument/didChange"),
        1,
        "other.zig changed on disk while open; the server must be told"
    );

    std::fs::remove_file(&other).unwrap();
    ok(&p, &["outline", &file]);
    assert_eq!(
        count(&log_of(&p), "textDocument/didClose"),
        1,
        "a deleted open file must be closed, not served from memory"
    );
}

#[test]
fn at_most_sixteen_documents_stay_open() {
    let p = project("sync-lru", "fn main_fn() {}\n");
    let mut files = vec![];
    for i in 0..20 {
        let f = p.dir.join(format!("f{i}.zig"));
        std::fs::write(&f, format!("fn f{i}() {{}}\n")).unwrap();
        files.push(f.display().to_string());
    }
    ok(&p, &["outline", &p.file()]);
    for f in &files {
        ok(&p, &["outline", f]);
    }
    let log = log_of(&p);
    let opened = count(&log, "textDocument/didOpen");
    let closed = count(&log, "textDocument/didClose");
    assert_eq!(opened, 21, "{log:?}");
    assert_eq!(opened - closed, 16, "open documents: {}", opened - closed);
}

/// A warm command against an unchanged file used to cost a fixed 3s
/// sleep, and a genuinely empty answer another 3s of retries.
#[test]
fn warm_commands_do_not_wait_when_nothing_changed() {
    let p = project("sync-latency", "fn a() {}\n");
    let file = p.file();
    ok(&p, &["outline", &file]); // cold start
    let started = std::time::Instant::now();
    ok(&p, &["outline", &file]);
    // The fake server has no definition provider: the answer is null.
    let def = ok(&p, &["definition", &file, "--scope", "1"]);
    let took = started.elapsed();
    assert!(def.contains("\"locations\":[]"), "{def}");
    assert!(
        took < std::time::Duration::from_millis(1500),
        "two warm commands took {took:?}"
    );
}

/// A file with no symbols (a barrel of re-exports, an empty module) used to
/// make the cold-start readiness wait poll for symbols until its 60s
/// deadline.
#[test]
fn cold_outline_of_a_file_with_no_symbols_does_not_wait_out_the_deadline() {
    let p = project("sync-empty", "// nothing here\n");
    let started = std::time::Instant::now();
    let out = ok(&p, &["outline", &p.file()]);
    let took = started.elapsed();
    assert!(out.contains("\"items\":[]"), "{out}");
    assert!(
        took < std::time::Duration::from_secs(15),
        "cold outline of an empty file took {took:?}"
    );
}

/// `server list` reports what a server *is*; `just_started` describes one
/// `/create` response and was leaking into every listing forever.
#[test]
fn server_list_does_not_report_every_server_as_just_started() {
    let p = project("list-flags", "fn a() {}\n");
    ok(&p, &["outline", &p.file()]);
    let list = ok(&p, &["server", "list", "--output", "json"]);
    assert!(list.contains("\"running\""), "{list}");
    assert!(!list.contains("just_started"), "{list}");
    assert!(!list.contains("loading"), "{list}");
}

/// The item `prepareCallHierarchy` returns must go back to the server
/// unchanged: servers keep state in its `data` field. It used to be decoded
/// into a struct without `data` and re-encoded, so a server relying on it
/// rejected the follow-up request.
#[test]
fn call_hierarchy_items_reach_the_server_with_their_data_intact() {
    let p = project("calls-data", "fn target() {}\n");
    let out = ok(
        &p,
        &[
            "calls",
            &p.file(),
            "--scope",
            "target",
            "--direction",
            "incoming",
        ],
    );
    assert!(out.contains("caller_of_target"), "{out}");
}

/// A server answering `documentSymbol` in the flat `SymbolInformation[]`
/// form got an empty outline: the reply failed to decode and the error was
/// swallowed.
#[test]
fn a_flat_document_symbol_reply_still_produces_an_outline() {
    let p = project("flat-symbols", "fn alpha() {}\nfn beta() {}\n");
    p.set_env("FAKE_LSP_FLAT_SYMBOLS", "1");
    let out = ok(&p, &["outline", &p.file()]);
    let data: serde_json::Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha", "beta"], "{data}");
}

/// `--project` names the root for a file with no root marker above it. The
/// CLI accepted that, but the daemon re-detected from markers alone and
/// failed with "Cannot detect language for path".
#[test]
fn project_override_works_for_a_file_with_no_root_marker() {
    let p = project("no-marker", "fn loose() {}\n");
    std::fs::remove_file(p.dir.join("build.zig")).unwrap();
    let dir = p.dir.display().to_string();
    let without = p.run(&["outline", &p.file()]);
    assert_eq!(
        without.exit_code, 1,
        "no marker and no --project: {}",
        without.stdout
    );
    let out = ok(&p, &["outline", &p.file(), "--project", &dir]);
    assert!(out.contains("\"loose\""), "{out}");
}

/// A relative path given to `server start` was resolved against the
/// daemon's working directory, not the caller's.
#[test]
fn server_start_resolves_a_relative_path_against_the_caller() {
    let p = project("start-relative", "fn a() {}\n");
    // Daemon started from somewhere else entirely.
    let r = p.run(&["server", "list"]);
    assert_eq!(r.exit_code, 0);

    let output = std::process::Command::new(support::bin_path())
        .args(["server", "start", "proj", "--output", "json"])
        .current_dir(p.home.path())
        .env("LSP_CLI_HOME", p.home.path())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", p.home.path().join("fakebin").display()),
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let root = std::fs::canonicalize(&p.dir).unwrap();
    assert_eq!(
        data["server"]["project_root"],
        root.display().to_string(),
        "{data}"
    );
}

/// `search` used to start a language server for the first source file it
/// found — a full rust-analyzer as a side effect of a name lookup. With
/// nothing warm it now answers from its own index and starts nothing.
#[test]
fn search_never_starts_a_server_and_uses_one_that_is_warm() {
    let p = project("search-warm", "fn needle_here() {}\n");
    let log = p.home.path().join("server.log");
    let r = p.run_in(&p.dir, &["search", "needle"]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let data: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(data["backend"], "bm25", "{data}");
    assert_eq!(data["items"][0]["name"], "needle_here", "{data}");
    assert!(!log.exists(), "search started a language server");

    // Once a navigation command has warmed one, search asks it.
    ok(&p, &["outline", &p.file()]);
    let r = p.run_in(&p.dir, &["search", "needle"]);
    let data: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(data["backend"], "lsp", "{data}");
    assert_eq!(data["items"][0]["containerName"], "from-fake-lsp", "{data}");
}

/// From a subdirectory, search used to treat that subdirectory as the
/// project (it only probed for index.ts/main.go/main.py in the current
/// directory).
#[test]
fn search_from_a_subdirectory_covers_the_whole_project() {
    let p = project("search-subdir", "fn top_level_thing() {}\n");
    let sub = p.dir.join("deep").join("er");
    std::fs::create_dir_all(&sub).unwrap();
    let r = p.run_in(&sub, &["search", "top_level"]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let data: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(data["items"][0]["name"], "top_level_thing", "{data}");
}

/// Outside any project, search indexed whatever directory it was run
/// from — all of /tmp, say. For the temp dir, home and / it now refuses.
#[test]
fn search_outside_any_project_refuses_to_index_the_temp_dir() {
    let p = project("search-tmp", "fn a() {}\n");
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    if support::find_marker_upwards(&tmp) {
        eprintln!("skipping: the temp dir is itself inside a project");
        return;
    }
    let r = p.run_in(&tmp, &["search", "anything"]);
    assert_eq!(r.exit_code, 1, "{}", r.stdout);
    assert!(r.stderr.contains("--project"), "{}", r.stderr);
}

/// A stray `~/package.json` used to win over a repository's `.git` one
/// level up, so searching from inside the repository indexed the whole
/// home directory. The nearest root marker or `.git` wins, and a root that
/// turns out to be the home directory itself is refused.
#[test]
fn search_prefers_the_nearest_root_and_never_indexes_home() {
    let p = project("search-home", "fn a() {}\n");
    let home = p.home.path().join("fakehome");
    let repo = home.join("code").join("scripts");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(home.join("Downloads")).unwrap();
    std::fs::write(home.join("package.json"), "{}").unwrap();
    std::fs::write(repo.join("tool.py"), "def needle_in_repo():\n    pass\n").unwrap();
    std::fs::write(home.join("Downloads").join("b.py"), "def needle_elsewhere():\n    pass\n").unwrap();
    p.set_env("HOME", &home.display().to_string());

    let r = p.run_in(&repo, &["search", "needle"]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("needle_in_repo"), "{}", r.stdout);
    assert!(!r.stdout.contains("needle_elsewhere"), "indexed beyond the repo: {}", r.stdout);

    let r = p.run_in(&home.join("Downloads"), &["search", "needle"]);
    assert_eq!(r.exit_code, 1, "searched the home directory: {}", r.stdout);
    assert!(r.stderr.contains("home directory"), "{}", r.stderr);
}
