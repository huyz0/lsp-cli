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

/// Pagination slicing lives inline in `run_search`, so these exercise it
/// through the CLI. It previously had no coverage that ran unconditionally
/// — `tests/reference.rs`'s one pagination test is gated on a language
/// server *and* self-skips when the fixture yields fewer than two results.
mod pagination {
    use super::*;

    fn page(args: &[&str]) -> serde_json::Value {
        let mut full = vec!["search", "e"];
        full.extend_from_slice(args);
        full.extend_from_slice(&["--project", env!("CARGO_MANIFEST_DIR")]);
        support::lsp_json(&full)
    }

    #[test]
    fn reports_total_and_start_index() {
        let data = page(&["--max-items", "2"]);
        assert_eq!(data["kind"], "search");
        assert!(data["total"].is_number());
        assert_eq!(data["startIndex"], 0);
    }

    #[test]
    fn max_items_caps_the_page() {
        let data = page(&["--max-items", "2"]);
        assert!(data["items"].as_array().unwrap().len() <= 2);
    }

    #[test]
    fn a_start_index_past_the_end_returns_an_empty_page_not_an_error() {
        let data = page(&["--start-index", "100000", "--max-items", "5"]);
        assert_eq!(data["items"].as_array().unwrap().len(), 0);
        assert_eq!(data["startIndex"], 100000);
    }

    #[test]
    fn a_huge_start_index_does_not_overflow() {
        // `start_index + max_items` was computed unchecked, which panics in
        // a debug build and wraps in release.
        let data = page(&["--start-index", "18446744073709551615", "--max-items", "20"]);
        assert_eq!(data["items"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn pages_do_not_overlap() {
        let first = page(&["--max-items", "2", "--start-index", "0"]);
        let second = page(&["--max-items", "2", "--start-index", "2"]);
        let a = first["items"].as_array().unwrap();
        let b = second["items"].as_array().unwrap();
        if !a.is_empty() && !b.is_empty() {
            assert_ne!(a[0], b[0], "page 2 should not repeat page 1's first item");
        }
    }

    #[test]
    fn an_unknown_kind_is_rejected_rather_than_silently_matching_nothing() {
        // It used to filter everything away and exit 0, which is
        // indistinguishable from a genuine "no such symbol".
        let result = support::lsp(&[
            "search",
            "e",
            "--kinds",
            "klass",
            "--project",
            env!("CARGO_MANIFEST_DIR"),
        ]);
        assert_ne!(result.exit_code, 0);
        assert!(
            result.stderr.contains("Unknown --kinds value"),
            "expected a naming error, got: {:?}",
            result.stderr
        );
    }
}
