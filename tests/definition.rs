mod support;
use support::{has_ts_server, lsp_json, ts_fixture};

#[test]
fn finds_definition_of_user_imported_type() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&["definition", service.to_str().unwrap(), "--scope", "createUser", "--find", ": <|>User"]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations[0]["uri"].as_str().unwrap().contains("models.ts"));
}
