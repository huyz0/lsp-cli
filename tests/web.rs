mod support;
use support::{has_css_server, has_html_server, has_json_server, lsp, lsp_json, web_fixture};

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
    let _ = lsp(&["server", "shutdown"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
}

#[test]
fn css_outline_returns_selector() {
    if !has_css_server() {
        eprintln!("skipping: vscode-css-language-server not installed");
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
        eprintln!("skipping: vscode-css-language-server not installed");
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
        eprintln!("skipping: vscode-json-language-server not installed");
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
fn html_outline_returns_valid_shape() {
    if !has_html_server() {
        eprintln!("skipping: vscode-html-language-server not installed");
        return;
    }
    let html = web_fixture("index.html");
    // vscode-html-language-server returns flat `SymbolInformation[]` (a
    // `location` field) rather than hierarchical `DocumentSymbol[]` (a
    // `range`/`selectionRange` pair) for textDocument/documentSymbol. The
    // outline command only deserializes the hierarchical shape — a
    // limitation inherited unchanged from the TS original's outline.ts,
    // which is typed strictly as `DocumentSymbol[]` there too. So this
    // command exits cleanly with an empty item list rather than an error;
    // that's the expected, if unhelpful, behavior for this server.
    let data = lsp_json(&["outline", html.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    assert!(data["items"].is_array());
}
