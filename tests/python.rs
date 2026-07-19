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
    let names: Vec<&str> = data["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"User"), "expected User in {names:?}");
}

#[test]
fn definition_follows_cross_file_import() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let service = py_fixture("src/service.py");
    let data = lsp_json(&["definition", service.to_str().unwrap(), "--scope", "create_user", "--find", "<|>User("]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations[0]["uri"].as_str().unwrap().contains("models.py"));
}

#[test]
fn reference_finds_usages_across_workspace() {
    if !has_basedpyright() {
        eprintln!("skipping: basedpyright-langserver not installed");
        return;
    }
    let models = py_fixture("src/models.py");
    let data = lsp_json(&["reference", models.to_str().unwrap(), "--scope", "User", "--max-items", "50"]);
    assert_eq!(data["kind"], "reference");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations.iter().any(|l| l["uri"].as_str().unwrap().contains("service.py")));
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
    assert!(!data["content"].as_str().unwrap().is_empty());
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
