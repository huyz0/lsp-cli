mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

/// Asserts `symbol` returns exactly lines `first..=last` (1-based) of
/// `file`, says so in `line`/`endLine`, and names the file. The old tests
/// checked that the source contained a word or two, so an off-by-one slice
/// (or the whole file) passed.
fn assert_symbol(file: &str, scope: &str, name: &str, kind: &str, first: u64, last: u64) {
    let path = ts_fixture(file);
    let data = lsp_json(&["symbol", path.to_str().unwrap(), "--scope", scope]);
    assert_eq!(data["kind"], "symbol");
    assert_eq!(data["name"], name, "{data}");
    assert_eq!(data["symbolKind"], kind, "{data}");
    assert_eq!(
        (data["line"].as_u64(), data["endLine"].as_u64()),
        (Some(first), Some(last)),
        "{data}"
    );
    assert_eq!(data["uri"], path.canonicalize().unwrap().to_str().unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    let expected: Vec<&str> = text
        .split('\n')
        .skip(first as usize - 1)
        .take((last - first + 1) as usize)
        .collect();
    assert_eq!(data["source"], expected.join("\n"));
}

#[test]
fn returns_exactly_the_source_of_a_class() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_symbol("src/models.ts", "User", "User", "class", 13, 35);
}

#[test]
fn returns_exactly_the_source_of_a_method_via_nested_scope() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_symbol("src/models.ts", "User.greet", "greet", "method", 25, 27);
}

#[test]
fn returns_exactly_the_source_of_a_top_level_function() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_symbol(
        "src/service.ts",
        "createUser",
        "createUser",
        "function",
        7,
        9,
    );
}

#[test]
fn markdown_output_contains_source_in_code_block() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&[
        "symbol",
        models.to_str().unwrap(),
        "--scope",
        "User.greet",
        "--output",
        "markdown",
    ]);
    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("greet [method]") && result.stdout.contains("models.ts:25-27"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("```\n  greet(): string {"),
        "{}",
        result.stdout
    );
}

#[test]
fn exits_cleanly_when_no_symbol_at_location() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    // Line 3 is a blank line inside the JSDoc — no symbol there.
    let result = lsp(&[
        "symbol",
        models.to_str().unwrap(),
        "--scope",
        "3",
        "--output",
        "json",
    ]);
    // A wrong position is an error: exit 1, message on stderr, nothing on
    // stdout for a caller to mistake for a result.
    assert_eq!(result.exit_code, 1, "stdout: {}", result.stdout);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("No symbol found at line 3"),
        "unexpected message: {}",
        result.stderr
    );
}
