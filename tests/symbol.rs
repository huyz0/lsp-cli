mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

#[test]
fn returns_full_source_of_a_class() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["symbol", models.to_str().unwrap(), "--scope", "User"]);
    assert_eq!(data["kind"], "symbol");
    assert_eq!(data["name"], "User");
    assert_eq!(data["symbolKind"], "class");
    let source = data["source"].as_str().unwrap();
    assert!(source.contains("greet"));
    assert!(source.contains("constructor"));
}

#[test]
fn returns_full_source_of_a_method_via_nested_scope() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["symbol", models.to_str().unwrap(), "--scope", "User.greet"]);
    assert_eq!(data["kind"], "symbol");
    assert_eq!(data["name"], "greet");
    assert!(data["source"].as_str().unwrap().contains("Hello"));
}

#[test]
fn returns_source_of_top_level_function() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&["symbol", service.to_str().unwrap(), "--scope", "createUser"]);
    assert_eq!(data["kind"], "symbol");
    assert_eq!(data["name"], "createUser");
    assert!(data["source"].as_str().unwrap().contains("new User"));
}

#[test]
fn markdown_output_contains_source_in_code_block() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&[
        "symbol",
        models.to_str().unwrap(),
        "--scope",
        "User.greet",
        "--output",
        "markdown",
    ]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("```"));
    assert!(result.stdout.contains("greet"));
}

#[test]
fn exits_cleanly_when_no_symbol_at_location() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    // Line 3 is a blank line inside the JSDoc — no symbol there.
    let result = lsp(&[
        "symbol",
        models.to_str().unwrap(),
        "--scope",
        "3",
        "--output",
        "json",
    ]);
    // This used to assert nothing at all — it could only fail by hanging.
    // `symbol` now reports "nothing here" the way `doc` and `rename` do:
    // a formatted result on stdout, exit 0.
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    let data: serde_json::Value = serde_json::from_str(&result.stdout)
        .unwrap_or_else(|e| panic!("expected JSON on stdout: {e}\nstdout: {}", result.stdout));
    assert_eq!(data["kind"], "error");
    assert!(
        data["message"]
            .as_str()
            .is_some_and(|m| m.contains("No symbol found")),
        "unexpected message: {:?}",
        data["message"]
    );
}
