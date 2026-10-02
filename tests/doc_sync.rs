//! A warm server keeps every file a command has touched open, and for an
//! open document the server's in-memory copy wins over the disk. These
//! tests edit a *different* file than the one being queried, after it was
//! opened by an earlier command, and check the answers reflect the disk —
//! the situation an agent creates constantly (edit a file, then query
//! another one).
//!
//! Each test uses its own copy of the TypeScript fixture, since it writes.

mod support;
use support::{has_ts_server, locations, lsp_json, prepend, ts_project_copy};

#[test]
fn definition_tracks_an_on_disk_edit_to_an_already_open_file() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_project_copy();
    let models = project.path().join("src/models.ts");
    let service = project.path().join("src/service.ts");
    // Opens models.ts in the warm server.
    lsp_json(&["outline", models.to_str().unwrap()]);

    prepend(&models, "// one\n// two\n");

    let data = lsp_json(&[
        "definition",
        service.to_str().unwrap(),
        "--scope",
        "createUser",
        "--find",
        "): <|>User",
    ]);
    assert_eq!(
        locations(&data),
        vec![("models.ts".to_string(), 15, 13)],
        "the class moved down two lines on disk"
    );
}

#[test]
fn diagnostics_see_an_on_disk_edit_to_an_already_open_file() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_project_copy();
    let models = project.path().join("src/models.ts");
    let service = project.path().join("src/service.ts");
    lsp_json(&["outline", models.to_str().unwrap()]);
    let clean = lsp_json(&["diagnostics", service.to_str().unwrap()]);
    assert_eq!(clean["items"].as_array().map(Vec::len), Some(0), "{clean}");

    // Break service.ts's import by renaming the class it imports.
    let text = std::fs::read_to_string(&models).unwrap();
    std::fs::write(&models, text.replace("class User", "class Person")).unwrap();

    let broken = lsp_json(&["diagnostics", service.to_str().unwrap()]);
    let messages: Vec<&str> = broken["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("User")),
        "expected an error about the missing `User` export, got {messages:?}"
    );
}

/// The worst case of a stale open document: rename computes its edits
/// against the server's outdated copy, and `--apply` writes them at the
/// wrong offsets into the real file. Reproduced: index.ts became
/// `export { makeUser as createUserm "./models";`.
#[test]
fn rename_after_an_edit_to_an_already_open_file_writes_correct_text() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_project_copy();
    let index = project.path().join("src/index.ts");
    let service = project.path().join("src/service.ts");
    lsp_json(&["outline", index.to_str().unwrap()]);

    prepend(&index, "// header\n// added after it was opened\n");

    let data = lsp_json(&[
        "rename",
        service.to_str().unwrap(),
        "--scope",
        "createUser",
        "--find",
        "function <|>createUser",
        "--new-name",
        "makeUser",
        "--apply",
    ]);
    assert_eq!(data["applied"], true, "{data}");
    assert_eq!(
        std::fs::read_to_string(&index).unwrap(),
        "// header\n// added after it was opened\n\
         export { User } from \"./models\";\n\
         export type { UserOptions, UserId } from \"./models\";\n\
         export { makeUser as createUser, findUser, greetUser } from \"./service\";\n",
        "TypeScript keeps the re-export's public name by aliasing it"
    );
    let service_text = std::fs::read_to_string(&service).unwrap();
    assert!(service_text.contains("export function makeUser(options: UserOptions): User {"));
    assert!(service_text.contains("    return makeUser({ name: \"Alice\""));
    assert!(!service_text.contains("createUser"), "{service_text}");
}

/// A file deleted on disk after being opened must not keep answering from
/// the server's memory.
#[test]
fn outline_of_a_recreated_file_reflects_its_new_content() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_project_copy();
    let models = project.path().join("src/models.ts");
    let service = project.path().join("src/service.ts");
    lsp_json(&["outline", models.to_str().unwrap()]);
    std::fs::write(&models, "export class Replacement {}\n").unwrap();
    let data = lsp_json(&["diagnostics", service.to_str().unwrap()]);
    let messages: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("User")),
        "service.ts imports names models.ts no longer has: {messages:?}"
    );
}

/// typescript-language-server only publishes diagnostics when they change.
/// After an edit that doesn't affect a file, its diagnostics call used to
/// wait the full publish timeout — and again on every later call, since
/// nothing recorded that the wait had already happened.
#[test]
fn diagnostics_after_an_unrelated_change_wait_at_most_once() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_project_copy();
    let models = project.path().join("src/models.ts");
    let service = project.path().join("src/service.ts");
    lsp_json(&["diagnostics", service.to_str().unwrap()]);
    lsp_json(&["outline", models.to_str().unwrap()]); // opens another file

    let timed = || {
        let started = std::time::Instant::now();
        let data = lsp_json(&["diagnostics", service.to_str().unwrap()]);
        assert_eq!(data["items"].as_array().map(Vec::len), Some(0), "{data}");
        started.elapsed()
    };
    let first = timed();
    let second = timed();
    let third = timed();
    assert!(
        first < std::time::Duration::from_millis(3500),
        "first call after an unrelated change took {first:?}"
    );
    for took in [second, third] {
        assert!(
            took < std::time::Duration::from_millis(1000),
            "nothing changed since the last call, yet it took {took:?}"
        );
    }
}
