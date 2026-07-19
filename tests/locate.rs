mod support;
use support::{lsp, lsp_json, ts_fixture};

#[test]
fn resolves_a_line_number() {
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["locate", models.to_str().unwrap(), "--scope", "5"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains('5'));
}

#[test]
fn resolves_a_symbol_path() {
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["locate", models.to_str().unwrap(), "--scope", "User"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
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
    let result = lsp(&["locate", models.to_str().unwrap(), "--scope", "1,5", "--find", "DOES_NOT_EXIST_XYZ"]);
    assert_eq!(result.exit_code, 1);
}

#[test]
fn exits_1_when_file_does_not_exist() {
    let result = lsp(&["locate", "/nonexistent/file.ts", "--scope", "1"]);
    assert_eq!(result.exit_code, 1);
}
