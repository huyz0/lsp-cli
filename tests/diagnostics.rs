mod support;
use support::{has_ts_server, lsp_json, ts_fixture};

/// Deletes the fixture file on drop, so it's cleaned up even if an
/// assertion below panics mid-test.
struct CleanupOnDrop(std::path::PathBuf);
impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn reports_a_real_type_error() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let broken = ts_fixture("src/diagnostics_check.ts");
    std::fs::write(&broken, "const result: string = 1 + 1;\n").unwrap();
    let _cleanup = CleanupOnDrop(broken.clone());

    let data = lsp_json(&["diagnostics", broken.to_str().unwrap()]);
    assert_eq!(data["kind"], "diagnostics");
    let items = data["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected at least one diagnostic, got {items:?}");
    assert_eq!(items[0]["severity"], "error");
    assert!(items[0]["message"].as_str().unwrap().contains("not assignable"), "unexpected message: {}", items[0]["message"]);
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
