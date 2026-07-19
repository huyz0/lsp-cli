mod support;
use support::{has_lua_language_server, lsp_json, lua_fixture};

#[test]
fn outline_returns_local_function() {
    if !has_lua_language_server() {
        eprintln!("skipping: lua-language-server not installed");
        return;
    }
    let main_lua = lua_fixture("main.lua");
    let data = lsp_json(&["outline", main_lua.to_str().unwrap(), "--all"]);
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
}

#[test]
fn doc_returns_hover_for_function() {
    if !has_lua_language_server() {
        eprintln!("skipping: lua-language-server not installed");
        return;
    }
    let main_lua = lua_fixture("main.lua");
    let data = lsp_json(&[
        "doc",
        main_lua.to_str().unwrap(),
        "--scope",
        "1",
        "--find",
        "local function <|>add",
    ]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("add"));
}
