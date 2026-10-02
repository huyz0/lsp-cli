mod support;
use support::{lsp, lsp_json, ts_fixture};

// These assert on the resolved line number from the JSON output rather
// than searching the rendered markdown for a substring. `contains('5')`
// passed for any line 2-8, because the markdown prints three lines of
// context either side with a line-number gutter; `contains("User")` passed
// for a resolution landing anywhere near any of the eight occurrences of
// "User" in the fixture. Both would have accepted a badly broken resolver.

#[test]
fn resolves_a_line_number() {
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["locate", models.to_str().unwrap(), "--scope", "5"]);
    assert_eq!(data["line"], 5);
}

#[test]
fn resolves_a_symbol_path_to_the_declaration_line() {
    let models = ts_fixture("src/models.ts");
    let source = std::fs::read_to_string(&models).unwrap();
    let expected = source
        .lines()
        .position(|l| l.contains("class User"))
        .expect("fixture should declare `class User`") as i64
        + 1;

    let data = lsp_json(&["locate", models.to_str().unwrap(), "--scope", "User"]);
    assert_eq!(
        data["line"], expected,
        "expected the `class User` declaration line, got {}",
        data["line"]
    );
}

#[test]
fn find_resolves_to_the_matching_line_not_merely_somewhere_nearby() {
    let models = ts_fixture("src/models.ts");
    let source = std::fs::read_to_string(&models).unwrap();
    // The method itself, not the earlier doc comment "Returns a greeting
    // message", which contains "greet" only as part of another word.
    let (idx, line) = source
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("greet(): string"))
        .expect("fixture should declare `greet`");

    let data = lsp_json(&["locate", models.to_str().unwrap(), "--find", "greet"]);
    assert_eq!(data["line"], idx as i64 + 1);
    // The character offset should land on `greet` itself.
    let col = data["character"].as_u64().unwrap() as usize;
    assert!(
        line[col..].starts_with("greet"),
        "character {col} of {line:?} is not the start of `greet`"
    );
}

#[test]
fn json_output_has_correct_shape() {
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["locate", models.to_str().unwrap(), "--scope", "1"]);
    assert_eq!(data["kind"], "locate");
    assert_eq!(data["line"], 1);
    assert!(data["file"].as_str().unwrap().contains("models.ts"));
}

#[test]
fn exits_1_when_pattern_not_found() {
    let models = ts_fixture("src/models.ts");
    let result = lsp(&[
        "locate",
        models.to_str().unwrap(),
        "--scope",
        "1,5",
        "--find",
        "DOES_NOT_EXIST_XYZ",
    ]);
    assert_eq!(result.exit_code, 1);
}

#[test]
fn exits_1_when_file_does_not_exist() {
    let result = lsp(&["locate", "/nonexistent/file.ts", "--scope", "1"]);
    assert_eq!(result.exit_code, 1);
}

/// `--scope 500,600` on a short file panicked (exit 101, "range start index
/// out of range"); a line past the end silently became the last line.
#[test]
fn an_out_of_range_scope_is_a_clean_error() {
    let file = support::fixture("go_project/go.mod");
    for scope in ["500,600", "500", "0"] {
        let result = support::lsp(&["locate", file.to_str().unwrap(), "--scope", scope]);
        assert_eq!(result.exit_code, 1, "--scope {scope}: {}", result.stderr);
        assert!(result.stderr.contains("out of range"), "{}", result.stderr);
    }
}

/// Without `--scope` or `--find` there is no position. `definition` used to
/// query line 1 anyway and exit 0 with an empty answer.
#[test]
fn navigation_without_a_position_is_an_error() {
    let file = support::ts_fixture("src/service.ts");
    let result = support::lsp(&["definition", file.to_str().unwrap()]);
    assert_eq!(result.exit_code, 1, "{}", result.stdout);
    assert!(result.stderr.contains("--scope"), "{}", result.stderr);
}

/// `UserOptions.greet` doesn't exist; it used to resolve to `User.greet`
/// further down the file.
#[test]
fn a_nested_scope_never_resolves_into_a_different_parent() {
    let file = support::ts_fixture("src/models.ts");
    let result = support::lsp(&[
        "locate",
        file.to_str().unwrap(),
        "--scope",
        "UserOptions.greet",
    ]);
    assert_eq!(result.exit_code, 1, "{}", result.stdout);
    assert!(
        result.stderr.contains("Nested symbol not found"),
        "{}",
        result.stderr
    );

    let data = support::lsp_json(&["locate", file.to_str().unwrap(), "--scope", "User.greet"]);
    assert_eq!(data["line"], 25, "{data}");
}
