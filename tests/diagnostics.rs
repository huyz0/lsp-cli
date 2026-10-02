mod support;
use support::{has_ts_server, lsp_json, ts_fixture, ts_project_copy};

#[test]
fn reports_a_real_type_error() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    // A private copy: writing into the shared fixture raced every other
    // test binary querying it.
    let project = ts_project_copy();
    let broken = project.path().join("src/diagnostics_check.ts");
    std::fs::write(&broken, "const result: string = 1 + 1;\n").unwrap();

    let data = lsp_json(&["diagnostics", broken.to_str().unwrap()]);
    assert_eq!(data["kind"], "diagnostics");
    let items = data["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "expected at least one diagnostic, got {items:?}"
    );
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["severity"], "error");
    assert_eq!(items[0]["line"], 1);
    assert!(
        items[0]["message"]
            .as_str()
            .unwrap()
            .contains("not assignable"),
        "unexpected message: {}",
        items[0]["message"]
    );
}

#[test]
fn a_clean_file_reports_no_diagnostics() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let clean = ts_fixture("src/models.ts");
    let data = lsp_json(&["diagnostics", clean.to_str().unwrap()]);
    assert_eq!(data["kind"], "diagnostics");
    assert_eq!(data["items"].as_array().unwrap().len(), 0);
}
