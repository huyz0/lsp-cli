mod support;
use support::{has_css_server, has_html_server, has_json_server, lsp_json, web_fixture};

/// The full `cargo test` run touches many different languages across many
/// test binaries, and — now that navigation commands reuse warm
/// daemon-managed servers instead of spawning fresh per call, with a
/// default 600s idle timeout — every server spawned anywhere earlier in the
/// suite can still be alive and competing for CPU by the time these tests
/// run. That's not a realistic single-session workload (a real user
/// touching TypeScript, Python, Go, Rust, Java, Kotlin, CSS, HTML, and JSON
/// all within the same few minutes is unusual), but it *is* what a
/// clean-slate full-suite run does, and it's enough concurrent contention to
/// make even a generous settle delay (see commands.rs) unreliable. Force a
/// clean slate before this file's own (deliberately concurrent: css + html +
/// json) servers get warm, so this file's assertions depend only on its own
/// three servers contending with each other — which is realistic and was
/// verified reliable in isolation — not on whatever every earlier test file
/// left running.
fn reset_daemon() {
    // Intentionally empty. This used to shut the daemon down to get a
    // clean slate from whatever earlier test *files* had left running —
    // which also killed the developer's own daemon. Each test binary now
    // gets its own LSP_CLI_HOME (see tests/support), so the isolation this
    // was reaching for is structural and no shutdown is needed. Kept as a
    // no-op so the call sites still read as "this file starts clean".
}

#[test]
fn css_outline_returns_selector() {
    if !has_css_server() {
        eprintln!("skipping: lsp-css-lsp not built");
        return;
    }
    reset_daemon();
    let css = web_fixture("styles.css");
    let data = lsp_json(&["outline", css.to_str().unwrap()]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&".greeting"),
        "expected .greeting in {names:?}"
    );
}

#[test]
fn css_doc_returns_hover_for_selector() {
    if !has_css_server() {
        eprintln!("skipping: lsp-css-lsp not built");
        return;
    }
    let css = web_fixture("styles.css");
    let data = lsp_json(&[
        "doc",
        css.to_str().unwrap(),
        "--scope",
        "1",
        "--find",
        ".<|>greeting",
    ]);
    assert_eq!(data["kind"], "hover");
    assert!(!data["content"].as_str().unwrap().is_empty());
}

#[test]
fn json_outline_returns_keys_with_all_flag() {
    if !has_json_server() {
        eprintln!("skipping: lsp-json-lsp not built");
        return;
    }
    // Force a clean slate right before this call specifically (rather than
    // relying on execution order relative to the other tests in this file)
    // — see the module doc comment on `reset_daemon`.
    reset_daemon();
    let json_file = web_fixture("data.json");
    // Top-level JSON keys are `key`/`string`-kind symbols, which the outline
    // command's default top-level filter (class/interface/enum/function/
    // module/namespace/struct — see commands.rs::filter_top_level, ported
    // from outline.ts) intentionally excludes; --all bypasses that filter.
    let data = lsp_json(&["outline", json_file.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"name"), "expected name in {names:?}");
    assert!(names.contains(&"version"), "expected version in {names:?}");
}

#[test]
fn html_outline_is_hierarchical_and_names_id_class() {
    if !has_html_server() {
        eprintln!("skipping: lsp-html-lsp not built");
        return;
    }
    // The npm-installed vscode-html-language-server this tool used to
    // proxy to returned flat `SymbolInformation[]` instead of hierarchical
    // `DocumentSymbol[]` for documentSymbol, so outline always came back
    // empty for HTML (a real, documented server limitation — see
    // docs/language-support.md). lsp-html-lsp (Rust-native, bundled, see
    // docs/architecture.md#bundled-rust-native-servers) fixes that: real
    // nested symbols, `tag#id.class` naming.
    let html = web_fixture("index.html");
    let data = lsp_json(&["outline", html.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let root = data["items"].as_array().unwrap();
    assert_eq!(root.len(), 1, "expected one root <html> symbol: {root:?}");
    assert_eq!(root[0]["name"], "html");
    let html_children = root[0]["children"].as_array().unwrap();
    let names: Vec<&str> = html_children
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"head"), "expected head in {names:?}");
    let body = html_children
        .iter()
        .find(|c| c["name"] == "body")
        .unwrap_or_else(|| panic!("expected body in {names:?}"));
    let body_children = body["children"].as_array().unwrap();
    let body_names: Vec<&str> = body_children
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        body_names.contains(&"h1#greeting"),
        "expected h1#greeting (tag#id naming) in {body_names:?}"
    );
}
