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
