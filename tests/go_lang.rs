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
    // `type User struct` at models.go:5, name at character 5.
    assert_eq!(
        support::locations(&data),
        vec![("models.go".to_string(), 5, 5)]
    );
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
    // The hover must be about *this* symbol: its signature and its own doc
    // comment. Checking for non-empty content (or the symbol's name, which
    // nearly any hover nearby contains) let a hover on the wrong symbol pass.
    let content = data["content"].as_str().unwrap();
    assert!(content.contains("type User struct"), "{content}");
    assert!(
        content.contains("User represents a user in the system."),
        "{content}"
    );
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
