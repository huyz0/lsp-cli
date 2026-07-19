mod support;
use support::{has_zls, lsp_json, zig_fixture};

#[test]
fn outline_returns_main_function() {
    if !has_zls() {
        eprintln!("skipping: zls not installed");
        return;
    }
    let main_zig = zig_fixture("main.zig");
    let data = lsp_json(&["outline", main_zig.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("main")),
        "expected main in {names:?}"
    );
}

#[test]
fn doc_returns_hover_for_function() {
    if !has_zls() {
        eprintln!("skipping: zls not installed");
        return;
    }
    let main_zig = zig_fixture("main.zig");
    let data = lsp_json(&[
        "doc",
        main_zig.to_str().unwrap(),
        "--scope",
        "1",
        "--find",
        "pub fn <|>main",
    ]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("main"));
}
