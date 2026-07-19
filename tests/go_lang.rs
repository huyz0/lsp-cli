mod support;
use support::{go_fixture, has_gopls, lsp, lsp_json};

#[test]
fn outline_returns_struct_and_methods() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let models = go_fixture("models.go");
    let data = lsp_json(&["outline", models.to_str().unwrap(), "--all"]);
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
fn definition_follows_cross_file_reference() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let service = go_fixture("service.go");
    let data = lsp_json(&[
        "definition",
        service.to_str().unwrap(),
        "--scope",
        "CreateUser",
        "--find",
        "return <|>User",
    ]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations[0]["uri"].as_str().unwrap().contains("models.go"));
}

#[test]
fn doc_returns_hover_for_struct() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let models = go_fixture("models.go");
    let data = lsp_json(&["doc", models.to_str().unwrap(), "--scope", "User"]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("User"));
}

#[test]
fn markdown_outline_contains_struct_name() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let models = go_fixture("models.go");
    let result = lsp(&["outline", models.to_str().unwrap(), "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
}
