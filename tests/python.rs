mod support;
use support::{has_basedpyright, lsp, lsp_json, py_fixture};

#[test]
fn outline_returns_class_with_methods() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let models = py_fixture("src/models.py");
    let data = lsp_json(&["outline", models.to_str().unwrap()]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"User"), "expected User in {names:?}");
}

#[test]
fn definition_follows_cross_file_import() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let service = py_fixture("src/service.py");
    let data = lsp_json(&[
        "definition",
        service.to_str().unwrap(),
        "--scope",
        "create_user",
        "--find",
        "<|>User(",
    ]);
    assert_eq!(data["kind"], "definition");
    // `class User` at models.py:8, name at character 6 — not merely "some
    // location in models.py".
    assert_eq!(
        support::locations(&data),
        vec![("models.py".to_string(), 8, 6)]
    );
}

#[test]
fn reference_finds_usages_across_workspace() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let models = py_fixture("src/models.py");
    let data = lsp_json(&[
        "reference",
        models.to_str().unwrap(),
        "--scope",
        "User",
        "--max-items",
        "50",
    ]);
    assert_eq!(data["kind"], "reference");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations
        .iter()
        .any(|l| l["uri"].as_str().unwrap().contains("service.py")));
}

#[test]
fn doc_returns_hover_for_method() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let models = py_fixture("src/models.py");
    let data = lsp_json(&["doc", models.to_str().unwrap(), "--scope", "User.greet"]);
    assert_eq!(data["kind"], "hover");
    // The hover must be about *this* symbol: its signature and its own doc
    // comment. Checking for non-empty content (or the symbol's name, which
    // nearly any hover nearby contains) let a hover on the wrong symbol pass.
    let content = data["content"].as_str().unwrap();
    assert!(content.contains("def greet(self"), "{content}");
    assert!(
        content.contains("Returns a greeting message for the user."),
        "{content}"
    );
}

#[test]
fn markdown_output_contains_class_name() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let models = py_fixture("src/models.py");
    let result = lsp(&["outline", models.to_str().unwrap(), "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
}
