mod support;
use support::{has_ts_server, lsp, lsp_json, ts_fixture};

/// `(name, kind, start line, end line)` for each item, children indented
/// by two spaces per level.
fn tree(items: &serde_json::Value) -> Vec<String> {
    fn walk(items: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
        for i in items.as_array().into_iter().flatten() {
            out.push(format!(
                "{}{} {} {}-{}",
                "  ".repeat(depth),
                i["name"].as_str().unwrap(),
                i["kind"].as_str().unwrap(),
                i["range"]["start"]["line"],
                i["range"]["end"]["line"]
            ));
            walk(&i["children"], depth + 1, out);
        }
    }
    let mut out = vec![];
    walk(items, 0, &mut out);
    out
}

/// The whole tree, exactly: names, kinds, line ranges, nesting, and source
/// order. typescript-language-server answers alphabetically (`User` before
/// `UserOptions`, members as constructor/email/greet/name/toString); the
/// outline is put back in file order. This used to check only that the
/// name list contained "User".
#[test]
fn outline_is_the_exact_symbol_tree_in_source_order() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["outline", models.to_str().unwrap()]);
    assert_eq!(data["kind"], "outline");
    assert_eq!(
        tree(&data["items"]),
        [
            "UserOptions interface 5-8",
            "  name property 6-6",
            "  email property 7-7",
            "User class 13-35",
            "  name property 14-14",
            "  email property 15-15",
            "  constructor constructor 17-20",
            "  greet method 25-27",
            "  toString method 32-34",
        ]
    );
}

/// `--all` adds what the default view filters out: here the `UserId` type
/// alias, which the server reports as a variable. The old test asserted
/// `UserOptions`, which the default view shows too, so it could not fail.
#[test]
fn all_flag_adds_what_the_default_view_filters_out() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let data = lsp_json(&["outline", models.to_str().unwrap(), "--all"]);
    assert_eq!(
        tree(&data["items"]),
        [
            "UserOptions interface 5-8",
            "  name property 6-6",
            "  email property 7-7",
            "User class 13-35",
            "  name property 14-14",
            "  email property 15-15",
            "  constructor constructor 17-20",
            "  greet method 25-27",
            "  toString method 32-34",
            "UserId variable 37-37",
        ]
    );
}

#[test]
fn markdown_output_contains_class_name() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["outline", models.to_str().unwrap(), "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
}
