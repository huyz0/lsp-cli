mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

#[test]
fn finds_all_references_to_user_across_workspace() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let service = ts_fixture("src/service.ts");
    // Ensure service.ts is indexed by opening it first.
    lsp(&["outline", service.to_str().unwrap()]);

    let data = lsp_json(&[
        "reference",
        models.to_str().unwrap(),
        "--scope",
        "User",
        "--max-items",
        "50",
        "--pagination-id",
        "ref-test-1",
    ]);

    assert_eq!(data["kind"], "reference");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    let has_service_ref = locations.iter().any(|l| l["uri"].as_str().unwrap().contains("service.ts"));
    assert!(has_service_ref);
}

#[test]
fn pagination_page_1_and_2_are_distinct() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");

    let all = lsp_json(&["reference", models.to_str().unwrap(), "--scope", "User", "--max-items", "100", "--pagination-id", "ref-test-2"]);
    let all_locations = all["locations"].as_array().unwrap();
    if all_locations.len() < 2 {
        eprintln!("not enough results to paginate, skipping");
        return;
    }

    let page1 = lsp_json(&["reference", models.to_str().unwrap(), "--scope", "User", "--max-items", "1", "--start-index", "0", "--pagination-id", "ref-test-3"]);
    let page2 = lsp_json(&["reference", models.to_str().unwrap(), "--scope", "User", "--max-items", "1", "--start-index", "1", "--pagination-id", "ref-test-3"]);

    let loc1 = &page1["locations"].as_array().unwrap()[0];
    let loc2 = &page2["locations"].as_array().unwrap()[0];
    if loc1["uri"] == loc2["uri"] {
        assert_ne!(loc1["line"], loc2["line"]);
    } else {
        assert_ne!(loc1["uri"], loc2["uri"]);
    }
}

#[test]
fn mode_implementations_returns_valid_reference_shape() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["reference", models.to_str().unwrap(), "--scope", "User", "--mode", "implementations"]);
    assert_eq!(data["kind"], "reference");
}

#[test]
fn markdown_output_contains_file_paths() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["reference", models.to_str().unwrap(), "--scope", "User", "--max-items", "10", "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(regex_ts_line(&result.stdout));
}

fn regex_ts_line(s: &str) -> bool {
    // matches /\.ts:\d+/
    s.split("\n").any(|line| {
        if let Some(idx) = line.find(".ts:") {
            line[idx + 4..].chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        } else {
            false
        }
    })
}
