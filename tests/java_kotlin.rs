// jdtls (Java) and kotlin-language-server are both nontrivial to install
// standalone (jdtls ships as an Eclipse JDT workspace bundle; the Kotlin
// server needs a Gradle build from source or a release tarball) and neither
// is installed in this environment, so these tests are skip-gated exactly
// like the others and simply weren't exercised here. Fixtures are in place
// (`tests/fixtures/java_project/`, `tests/fixtures/kotlin_project/`) so they
// run for real wherever the servers are available.
mod support;
use support::{fixture, has_jdtls, has_kotlin_language_server, lsp_json};

#[test]
fn java_outline_returns_class_with_method() {
    if !has_jdtls() {
        eprintln!("skipping: jdtls not installed");
        return;
    }
    let user = fixture("java_project/src/main/java/com/example/User.java");
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
fn kotlin_outline_returns_class_with_method() {
    if !has_kotlin_language_server() {
        eprintln!("skipping: kotlin-language-server not installed");
        return;
    }
    let user = fixture("kotlin_project/src/main/kotlin/User.kt");
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
