//! `hierarchy` (type hierarchy) and its fallback. gopls implements type
//! hierarchy; typescript-language-server, basedpyright and rust-analyzer do
//! not. This command had no test that ran against any installed server.

mod support;
use support::{go_fixture, has_gopls, has_ts_server, locations, lsp, lsp_json, ts_fixture};

fn names_and_lines(data: &serde_json::Value) -> Vec<(String, u64)> {
    data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["name"].as_str().unwrap().to_string(),
                i["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn go_subtypes_of_an_interface_are_its_implementations() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let models = go_fixture("models.go");
    let data = lsp_json(&[
        "hierarchy",
        models.to_str().unwrap(),
        "--scope",
        "Greeter",
        "--direction",
        "subtypes",
    ]);
    assert_eq!(data["kind"], "hierarchy");
    assert_eq!(names_and_lines(&data), [("User".to_string(), 5)], "{data}");
}

#[test]
fn go_supertypes_of_a_struct_are_the_interfaces_it_satisfies() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let models = go_fixture("models.go");
    let data = lsp_json(&[
        "hierarchy",
        models.to_str().unwrap(),
        "--scope",
        "User",
        "--direction",
        "supertypes",
    ]);
    // Workspace types only: the standard library's fmt.Stringer is
    // satisfied too, and whether gopls lists it is its own business.
    let in_workspace: Vec<(String, u64)> = names_and_lines(&data)
        .into_iter()
        .filter(|(name, _)| name == "Greeter")
        .collect();
    assert_eq!(in_workspace, [("Greeter".to_string(), 21)], "{data}");
}

/// The documented fallback where type hierarchy is unsupported.
#[test]
fn reference_mode_implementations_finds_the_same_implementor() {
    if !has_gopls() {
        eprintln!("skipping: gopls not installed");
        return;
    }
    let models = go_fixture("models.go");
    let data = lsp_json(&[
        "reference",
        models.to_str().unwrap(),
        "--scope",
        "Greeter",
        "--mode",
        "implementations",
    ]);
    assert_eq!(locations(&data), vec![("models.go".to_string(), 5, 5)]);
}

/// An unsupported request says so in words, with the alternative, rather
/// than surfacing `LSP error -32601: Unhandled method ...`.
#[test]
fn an_unsupported_hierarchy_request_explains_itself() {
    if !has_ts_server() {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }
    let models = ts_fixture("src/models.ts");
    let result = lsp(&["hierarchy", models.to_str().unwrap(), "--scope", "User"]);
    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains(
            "The typescript language server does not support `textDocument/prepareTypeHierarchy`"
        ),
        "{}",
        result.stderr
    );
    assert!(
        result.stderr.contains("--mode implementations"),
        "{}",
        result.stderr
    );
}
