mod support;
use support::{has_ts_server, locations, lsp_json, ts_fixture};

fn definition(find: &str) -> Vec<(String, u64, u64)> {
    let service = ts_fixture("src/service.ts");
    let data = lsp_json(&[
        "definition",
        service.to_str().unwrap(),
        "--scope",
        "createUser",
        "--find",
        find,
    ]);
    assert_eq!(data["kind"], "definition");
    locations(&data)
}

/// `class User` is declared at models.ts:13 (1-based line), name starting
/// at character 13 (0-based). This test used to search for `": <|>User"`,
/// which first matches `options: UserOptions` and so resolved
/// `UserOptions` instead — it passed only because it checked nothing but
/// the file name.
#[test]
fn definition_of_the_return_type_is_the_class_declaration() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_eq!(
        definition("): <|>User"),
        vec![("models.ts".to_string(), 13, 13)]
    );
}

/// A `new` expression resolves to both the class and its constructor
/// (models.ts:17, `constructor` at character 2).
#[test]
fn definition_of_a_constructor_call_gives_class_and_constructor() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_eq!(
        definition("new <|>User"),
        vec![
            ("models.ts".to_string(), 13, 13),
            ("models.ts".to_string(), 17, 2)
        ]
    );
}

/// `--find` prefers a whole identifier in code: `": <|>User"` first
/// occurs inside `options: UserOptions`, but the match that stands alone is
/// the return type `): User`. It used to take the first textual match and
/// resolve `UserOptions` (models.ts:5) instead.
#[test]
fn find_prefers_a_whole_identifier_over_a_partial_one() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    assert_eq!(
        definition(": <|>User"),
        vec![("models.ts".to_string(), 13, 13)]
    );
}
