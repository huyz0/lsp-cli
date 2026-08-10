mod support;
use support::{bash_fixture, has_bash_language_server, lsp_json};

#[test]
fn doc_returns_hover_for_function_call() {
    if !has_bash_language_server() {
        eprintln!("skipping: lsp-bash-lsp not built");
        return;
    }
    let main_sh = bash_fixture("main.sh");
    let data = lsp_json(&[
        "doc",
        main_sh.to_str().unwrap(),
        "--scope",
        "6",
        "--find",
        "<|>greet",
    ]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("greet"));
}

#[test]
fn reference_finds_function_call_site() {
    if !has_bash_language_server() {
        eprintln!("skipping: lsp-bash-lsp not built");
        return;
    }
    let main_sh = bash_fixture("main.sh");
    let data = lsp_json(&[
        "reference",
        main_sh.to_str().unwrap(),
        "--scope",
        "2",
        "--find",
        "<|>greet",
    ]);
    assert_eq!(data["kind"], "reference");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
}

#[test]
fn outline_returns_function_symbol() {
    // The old npm-installed bash-language-server's documentSymbol support
    // returned nothing for typical scripts — confirmed live, see
    // docs/language-support.md. lsp-bash-lsp (Rust-native, bundled, see
    // docs/architecture.md#bundled-rust-native-servers) fixes that: real
    // function symbols.
    if !has_bash_language_server() {
        eprintln!("skipping: lsp-bash-lsp not built");
        return;
    }
    let main_sh = bash_fixture("main.sh");
    let data = lsp_json(&["outline", main_sh.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"greet"), "expected greet in {names:?}");
}

#[test]
fn definition_follows_function_call_to_its_definition() {
    if !has_bash_language_server() {
        eprintln!("skipping: lsp-bash-lsp not built");
        return;
    }
    let main_sh = bash_fixture("main.sh");
    let data = lsp_json(&[
        "definition",
        main_sh.to_str().unwrap(),
        "--scope",
        "6",
        "--find",
        "<|>greet",
    ]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    // line 2 (1-based) is `greet() {` in tests/fixtures/bash_project/main.sh
    assert_eq!(locations[0]["line"], 2);
}
