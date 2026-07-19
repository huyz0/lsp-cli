mod support;
use support::{has_ruby_lsp, lsp_json, ruby_fixture};

#[test]
fn outline_returns_class_with_members() {
    if !has_ruby_lsp() {
        eprintln!("skipping: ruby-lsp not installed (or bundle not on PATH — see CONTRIBUTING.md)");
        return;
    }
    let greeter = ruby_fixture("greeter.rb");
    let data = lsp_json(&["outline", greeter.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Greeter"), "expected Greeter in {names:?}");
}

#[test]
fn definition_follows_method_call() {
    if !has_ruby_lsp() {
        eprintln!("skipping: ruby-lsp not installed (or bundle not on PATH — see CONTRIBUTING.md)");
        return;
    }
    let greeter = ruby_fixture("greeter.rb");
    let data = lsp_json(&["definition", greeter.to_str().unwrap(), "--scope", "11", "--find", "Greeter.new(\"world\").<|>greet"]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
}
