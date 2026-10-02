mod support;
use support::{has_ts_server, locations, lsp, lsp_json, ts_fixture};

fn user_refs(extra: &[&str]) -> serde_json::Value {
    let models = ts_fixture("src/models.ts");
    let mut args = vec!["reference", models.to_str().unwrap(), "--scope", "User"];
    args.extend_from_slice(extra);
    lsp_json(&args)
}

fn as_set(data: &serde_json::Value) -> std::collections::BTreeSet<(String, u64, u64)> {
    locations(data).into_iter().collect()
}

/// Every reference to `class User`, and nothing else: the import and the
/// three uses in service.ts, and the re-export in index.ts. Not the string
/// `` `User ${id} not found` ``, and not the declaration itself (the
/// command asks for `includeDeclaration: false`). This used to check only
/// that *some* result was in service.ts.
#[test]
fn finds_exactly_the_references_to_user_across_the_workspace() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let data = user_refs(&["--max-items", "50"]);
    assert_eq!(data["kind"], "reference");
    let expected: std::collections::BTreeSet<(String, u64, u64)> = [
        ("service.ts", 1, 9),
        ("service.ts", 7, 50),
        ("service.ts", 8, 13),
        ("service.ts", 14, 38),
        ("index.ts", 1, 9),
    ]
    .into_iter()
    .map(|(f, l, c)| (f.to_string(), l, c))
    .collect();
    assert_eq!(as_set(&data), expected, "{data}");
    assert_eq!(data["total"], 5);
    assert_eq!(data["truncated"], false);
}

/// Pages must partition the full result: no overlap, nothing lost, and
/// `total`/`truncated` telling the caller when to stop. This used to
/// compare only the first item of two pages.
#[test]
fn pages_partition_the_full_result_and_report_truncation() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let all = user_refs(&["--max-items", "100"]);
    let total = all["total"].as_u64().unwrap();
    let mut seen = vec![];
    let mut start = 0;
    loop {
        let page = user_refs(&["--max-items", "2", "--start-index", &start.to_string()]);
        assert_eq!(page["total"], total);
        assert_eq!(page["startIndex"], start);
        seen.extend(locations(&page));
        if page["truncated"] == false {
            break;
        }
        start += 2;
        assert!(start < 100, "truncated never became false");
    }
    assert_eq!(seen.len() as u64, total, "pages overlapped or lost results");
    let unique: std::collections::BTreeSet<_> = seen.iter().cloned().collect();
    assert_eq!(unique, as_set(&all));
}

#[test]
fn mode_implementations_returns_valid_reference_shape() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&[
        "reference",
        models.to_str().unwrap(),
        "--scope",
        "User",
        "--mode",
        "implementations",
    ]);
    assert_eq!(data["kind"], "reference");
}

#[test]
fn markdown_output_contains_file_paths() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&[
        "reference",
        models.to_str().unwrap(),
        "--scope",
        "User",
        "--max-items",
        "10",
        "--output",
        "markdown",
    ]);
    assert_eq!(result.exit_code, 0);
    assert!(regex_ts_line(&result.stdout));
}

fn regex_ts_line(s: &str) -> bool {
    // matches /\.ts:\d+/
    s.split("\n").any(|line| {
        if let Some(idx) = line.find(".ts:") {
            line[idx + 4..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        } else {
            false
        }
    })
}
