mod support;
use support::{has_rust_analyzer, lsp, lsp_json, rust_fixture};

#[test]
fn outline_returns_struct_and_impl_methods() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let user = rust_fixture("src/user.rs");
    let data = lsp_json(&["outline", user.to_str().unwrap(), "--all"]);
    assert_eq!(data["kind"], "outline");
    let names: Vec<&str> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"User"), "expected User in {names:?}");
}

#[test]
fn definition_follows_cross_file_use() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    // `locate`'s scope/find resolver uses regex heuristics ported from the TS
    // original (lsp/locate.ts) that only recognize class/function/def/func-style
    // declarations, not Rust's `struct`/`impl` keywords — so a dotted symbol
    // path like `--scope User` won't resolve here (same limitation exists in
    // the TS tool; not something this port introduced). Use a line/find scope
    // instead, which works for any language.
    let main_rs = rust_fixture("src/main.rs");
    let data = lsp_json(&[
        "definition",
        main_rs.to_str().unwrap(),
        "--scope",
        "5",
        "--find",
        "let u = <|>User",
    ]);
    assert_eq!(data["kind"], "definition");
    let locations = data["locations"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations[0]["uri"].as_str().unwrap().contains("user.rs"));
}

#[test]
fn doc_returns_hover_for_struct_via_line_scope() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let user = rust_fixture("src/user.rs");
    let data = lsp_json(&[
        "doc",
        user.to_str().unwrap(),
        "--scope",
        "2",
        "--find",
        "struct <|>User",
    ]);
    assert_eq!(data["kind"], "hover");
    assert!(data["content"].as_str().unwrap().contains("User"));
}

#[test]
fn markdown_outline_contains_struct_name() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    let user = rust_fixture("src/user.rs");
    let result = lsp(&["outline", user.to_str().unwrap(), "--output", "markdown"]);
    assert_eq!(result.exit_code, 0);
    assert!(result.stdout.contains("User"));
}

#[test]
fn rename_preview_finds_definition_and_call_site_without_writing_files() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    // No --apply: must not touch tests/fixtures/rust_project on disk (it's
    // shared with every other test in this file and checked into git).
    let user = rust_fixture("src/user.rs");
    let before = std::fs::read_to_string(&user).unwrap();
    let data = lsp_json(&[
        "rename",
        user.to_str().unwrap(),
        "--scope",
        "8",
        "--find",
        "fn <|>greet",
        "--new-name",
        "say_hello",
    ]);
    assert_eq!(data["kind"], "rename");
    assert_eq!(data["applied"], false);
    // 2 edits: the method definition itself, plus the call site in main.rs.
    assert_eq!(data["editCount"], 2);
    assert_eq!(data["files"].as_array().unwrap().len(), 2);
    let after = std::fs::read_to_string(&user).unwrap();
    assert_eq!(
        before, after,
        "preview (no --apply) must not modify the file"
    );
}

#[test]
fn rename_apply_writes_correct_edits_and_leaves_project_compiling() {
    if !has_rust_analyzer() {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }
    // Real disk writes — use an isolated tempdir copy, never the shared
    // fixture other tests in this file depend on.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"rename_apply_test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let main_rs = src.join("main.rs");
    std::fs::write(
        &main_rs,
        "fn greet() -> String {\n    \"hi\".to_string()\n}\n\nfn main() {\n    println!(\"{}\", greet());\n}\n",
    )
    .unwrap();

    let data = lsp_json(&[
        "rename",
        main_rs.to_str().unwrap(),
        "--scope",
        "1",
        "--find",
        "fn <|>greet",
        "--new-name",
        "say_hi",
        "--apply",
    ]);
    assert_eq!(data["kind"], "rename");
    assert_eq!(data["applied"], true);
    assert_eq!(data["editCount"], 2);

    let after = std::fs::read_to_string(&main_rs).unwrap();
    assert!(after.contains("fn say_hi()"), "{after}");
    assert!(after.contains("say_hi()"), "{after}");
    assert!(!after.contains("greet"), "old name should be gone: {after}");

    let build = std::process::Command::new("cargo")
        .args(["build", "--offline"])
        .current_dir(dir.path())
        .output();
    // --offline may fail in a sandbox with no cached std metadata available;
    // only assert success if cargo actually ran, don't fail the test on an
    // environment limitation unrelated to whether the rename itself is correct.
    if let Ok(build) = build {
        assert!(
            build.status.success(),
            "renamed project failed to compile:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
    }
}
