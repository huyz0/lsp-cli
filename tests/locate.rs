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
    let (idx, line) = source
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("greet"))
        .expect("fixture should contain `greet`");

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
