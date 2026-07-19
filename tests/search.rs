mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

#[test]
fn finds_user_symbol_in_typescript_project() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let project = ts_fixture("");
    // Ensure the TS server is running/indexed for this project before searching.
    lsp(&["outline", ts_fixture("src/models.ts").to_str().unwrap()]);

    let data = lsp_json(&["search", "User", "--project", project.to_str().unwrap()]);
    assert_eq!(data["kind"], "search");
    let items = data["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().any(|i| i["name"] == "User"));
}
