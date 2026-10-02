mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

/// Each result as `name@file:line -> [site file:line, ...]`.
fn summary(data: &serde_json::Value) -> Vec<String> {
    let file = |v: &serde_json::Value| {
        std::path::Path::new(v.as_str().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    };
    data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            let sites: Vec<String> = i["callSites"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| format!("{}:{}", file(&s["uri"]), s["line"]))
                .collect();
            format!(
                "{}@{}:{} -> {:?}",
                i["name"].as_str().unwrap(),
                file(&i["uri"]),
                i["line"],
                sites
            )
        })
        .collect()
}

fn calls(file: &str, scope: &str, direction: &str) -> serde_json::Value {
    let path = ts_fixture(file);
    let data = lsp_json(&[
        "calls",
        path.to_str().unwrap(),
        "--scope",
        scope,
        "--direction",
        direction,
    ]);
    assert_eq!(data["kind"], "calls");
    assert_eq!(data["direction"], direction);
    data
}

/// Exactly one caller, with the line of the call itself, not just of the
/// caller's declaration (which is all that used to be reported).
#[test]
fn incoming_gives_each_caller_and_its_call_sites() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_eq!(
        summary(&calls("src/service.ts", "createUser", "incoming")),
        [r#"findUser@service.ts:14 -> ["service.ts:16"]"#]
    );
    assert_eq!(
        summary(&calls("src/models.ts", "User.greet", "incoming")),
        [r#"greetUser@service.ts:24 -> ["service.ts:27"]"#]
    );
}

/// For outgoing calls the call sites are in the queried file, not the
/// callee's.
#[test]
fn outgoing_gives_each_callee_and_where_it_is_called_from() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_eq!(
        summary(&calls("src/service.ts", "createUser", "outgoing")),
        [r#"User@models.ts:13 -> ["service.ts:8"]"#]
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
    // No language-server gate: `--direction` is a clap value enum now, so
    // this is rejected at parse time, before the project root is resolved
    // or a server is contacted. It used to be a plain String checked
    // inside the async command body, which meant an invalid value did a
    // pile of work before failing.
    let result = lsp(&[
        "calls",
        "any-file.ts",
        "--scope",
        "createUser",
        "--direction",
        "sideways",
    ]);
    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("invalid value 'sideways'"),
        "unexpected stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("incoming") && result.stderr.contains("outgoing"),
        "the error should list the valid values: {}",
        result.stderr
    );
}
