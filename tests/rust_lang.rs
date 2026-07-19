mod support;
use support::{has_rust_analyzer, lsp, lsp_json, rust_fixture};

#[test]
fn outline_returns_struct_and_impl_methods() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let user = rust_fixture("src/user.rs");
    let data = lsp_json(&["outline", user.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"User"), "expected User in {names:?}");
}

#[test]
fn definition_follows_cross_file_use() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    // `locate`'s scope/find resolver uses regex heuristics ported from the TS
    // original (lsp/locate.ts) that only recognize class/function/def/func-style
    // declarations, not Rust's `struct`/`impl` keywords — so a dotted symbol
    // path like `--scope User` won't resolve here (same limitation exists in
    // the TS tool; not something this port introduced). Use a line/find scope
    // instead, which works for any language.
    let main_rs = rust_fixture("src/main.rs");
    let data = lsp_json(&["definition", main_rs.to_str().unwrap(), "--scope", "5", "--find", "let u = <|>User"]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations[0]["uri"].as_str().unwrap().contains("user.rs"));
}

#[test]
fn doc_returns_hover_for_struct_via_line_scope() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let user = rust_fixture("src/user.rs");
    let data = lsp_json(&["doc", user.to_str().unwrap(), "--scope", "2", "--find", "struct <|>User"]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("User"));
}

#[test]
fn markdown_outline_contains_struct_name() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let user = rust_fixture("src/user.rs");
    let result = lsp(&["outline", user.to_str().unwrap(), "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
}
