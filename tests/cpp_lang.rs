mod support;
use support::{cpp_fixture, has_clangd, lsp_json};

#[test]
fn outline_returns_function_signatures() {
    if !has_clangd() {
        eprintln!("skipping: clangd not installed");
        return;
    }
    let main_cpp = cpp_fixture("main.cpp");
    let data = lsp_json(&["outline", main_cpp.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("add")),
        "expected add in {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("main")),
        "expected main in {names:?}"
    );
}

#[test]
fn doc_returns_hover_for_function() {
    if !has_clangd() {
        eprintln!("skipping: clangd not installed");
        return;
    }
    let main_cpp = cpp_fixture("main.cpp");
    let data = lsp_json(&[
        "doc",
        main_cpp.to_str().unwrap(),
        "--scope",
        "1",
        "--find",
        "int <|>add",
    ]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("add"));
}
