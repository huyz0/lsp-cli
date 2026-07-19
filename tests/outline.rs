mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

#[test]
fn returns_symbol_tree_for_typescript_file() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["outline", models.to_str().unwrap()]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"User"), "expected User in {names:?}");
}

#[test]
fn all_flag_includes_interface_symbols() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["outline", models.to_str().unwrap(), "--all"]);
    let names: Vec<&str> = data["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"UserOptions"), "expected UserOptions in {names:?}");
}

#[test]
fn markdown_output_contains_class_name() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["outline", models.to_str().unwrap(), "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
}
