mod support;
use support::{bash_fixture, has_bash_language_server, lsp_json};

#[test]
fn doc_returns_hover_for_function_call() {
    if !has_bash_language_server() {
        eprintln!("skipping: bash-language-server not installed");
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
        eprintln!("skipping: bash-language-server not installed");
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
fn outline_returns_empty_list_known_server_limitation() {
    // bash-language-server's documentSymbol support returns nothing for
    // typical scripts — confirmed live, see docs/language-support.md.
    // This test locks in that documented (if unfortunate) behavior so a
    // future server upgrade that fixes it doesn't go unnoticed.
    if !has_bash_language_server() {
        eprintln!("skipping: bash-language-server not installed");
        return;
    }
    let main_sh = bash_fixture("main.sh");
    let data = lsp_json(&["outline", main_sh.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    assert!(data["items"].as_array().unwrap().is_empty());
}
