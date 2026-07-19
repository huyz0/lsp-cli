mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

#[test]
fn incoming_finds_the_caller() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&[
        "calls",
        service.to_str().unwrap(),
        "--scope",
        "createUser",
        "--direction",
        "incoming",
    ]);
    assert_eq!(data["kind"], "calls");
    assert_eq!(data["direction"], "incoming");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"findUser"),
        "expected findUser to call createUser, got {names:?}"
    );
}

#[test]
fn outgoing_finds_the_callee() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&[
        "calls",
        service.to_str().unwrap(),
        "--scope",
        "createUser",
        "--direction",
        "outgoing",
    ]);
    assert_eq!(data["kind"], "calls");
    assert_eq!(data["direction"], "outgoing");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"User"),
        "expected createUser to call the User constructor, got {names:?}"
    );
}

#[test]
fn a_symbol_with_no_callers_returns_an_empty_list() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&[
        "calls",
        service.to_str().unwrap(),
        "--scope",
        "greetUser",
        "--direction",
        "incoming",
    ]);
    assert_eq!(data["items"].as_array().unwrap().len(), 0);
}

#[test]
fn rejects_an_unknown_direction() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let result = lsp(&[
        "calls",
        service.to_str().unwrap(),
        "--scope",
        "createUser",
        "--direction",
        "sideways",
    ]);
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("Unknown direction"),
        "unexpected stderr: {}",
        result.stderr
    );
}
