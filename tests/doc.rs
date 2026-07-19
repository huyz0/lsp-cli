mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

#[test]
fn returns_hover_doc_for_documented_method() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["doc", models.to_str().unwrap(), "--scope", "User.greet"]);
    assert_eq!(data["kind"], "hover");
    assert!(!data["content"].as_str().unwrap().is_empty());
}

#[test]
fn returns_type_info_for_class() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["doc", models.to_str().unwrap(), "--scope", "User"]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("User"));
}

#[test]
fn markdown_output_is_non_empty() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["doc", models.to_str().unwrap(), "--scope", "User.greet", "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(!result.stdout.trim().is_empty());
}

#[test]
fn returns_doc_for_function_in_service() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&["doc", service.to_str().unwrap(), "--scope", "createUser"]);
    assert_eq!(data["kind"], "hover");
    assert!(!data["content"].as_str().unwrap().is_empty());
}

#[test]
fn dry_run_prints_lsp_request_without_executing() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["doc", models.to_str().unwrap(), "--scope", "User", "--dry-run"]);
    assert_eq!(result.exit_code, 0);
    let data: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(data["method"], "textDocument/hover");
}
