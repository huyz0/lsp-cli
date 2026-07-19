mod support;
use support::{csharp_fixture, has_csharp_ls, lsp_json};

#[test]
fn outline_returns_class_with_members() {
    if !has_csharp_ls() {
        eprintln!("skipping: csharp-ls not installed");
        return;
    }
    let greeter = csharp_fixture("Greeter.cs");
    let data = lsp_json(&["outline", greeter.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.iter().any(|n| n.contains("Greeter")), "expected Greeter in {names:?}");
}

#[test]
fn definition_follows_constructor_call() {
    if !has_csharp_ls() {
        eprintln!("skipping: csharp-ls not installed");
        return;
    }
    let greeter = csharp_fixture("Greeter.cs");
    let data = lsp_json(&["definition", greeter.to_str().unwrap(), "--scope", "22", "--find", "new <|>Greeter"]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
}

#[test]
fn doc_returns_hover_for_method() {
    if !has_csharp_ls() {
        eprintln!("skipping: csharp-ls not installed");
        return;
    }
    let greeter = csharp_fixture("Greeter.cs");
    let data = lsp_json(&["doc", greeter.to_str().unwrap(), "--scope", "12", "--find", "public string <|>Greet"]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("Greet"));
}
