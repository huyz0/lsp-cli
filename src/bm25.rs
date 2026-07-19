//! Self-contained BM25 scoring engine used as the fallback search path when no
//! LSP server workspace/symbol result is available (or no server is running
//! at all). Indexes symbols extracted with lightweight regex-based parsing
//! per language, then scores queries with the standard Okapi BM25 formula.

use std::collections::HashMap;
use walkdir::WalkDir;

use crate::protocol::{Location, Position, Range, SymbolInformation};

const K1: f64 = 1.5;
const B: f64 = 0.75;

#[derive(Debug, Clone)]
pub struct Doc {
    pub tokens: Vec<String>,
    pub symbol: SymbolInformation,
}

pub struct Bm25Index {
    docs: Vec<Doc>,
    doc_freq: HashMap<String, usize>,
    avg_len: f64,
}

fn tokenize(s: &str) -> Vec<String> {
    // Split camelCase / snake_case / kebab-case into lowercase word tokens.
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && !current.is_empty() {
                let prev = *chars.get(i.wrapping_sub(1)).unwrap_or(&' ');
                if prev.is_lowercase() || prev.is_numeric() {
                    tokens.push(current.to_lowercase());
                    current = String::new();
                }
            }
            current.push(c);
        } else {
            if !current.is_empty() {
                tokens.push(current.to_lowercase());
                current = String::new();
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens.retain(|t| !t.is_empty());
    tokens
}

/// Regex heuristics per language group, compiled exactly once (not once per
/// file — `extract_symbols` runs once per file in the project during BM25
/// indexing, and `regex::Regex::new` is not cheap; recompiling the same
/// handful of patterns for every file in a large project was pure waste).
struct LangPatterns {
    ts_js: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    py: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    go: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    rs: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    java_kt: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
}

static PATTERNS: LangPatterns = LangPatterns {
    ts_js: std::sync::OnceLock::new(),
    py: std::sync::OnceLock::new(),
    go: std::sync::OnceLock::new(),
    rs: std::sync::OnceLock::new(),
    java_kt: std::sync::OnceLock::new(),
};

fn patterns_for(ext: &str) -> &'static [(regex::Regex, u32)] {
    match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => PATTERNS.ts_js.get_or_init(|| {
            vec![
                (regex::Regex::new(r"^\s*(?:export\s+)?class\s+(\w+)").unwrap(), 5),
                (regex::Regex::new(r"^\s*(?:export\s+)?interface\s+(\w+)").unwrap(), 11),
                (regex::Regex::new(r"^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)").unwrap(), 12),
                (regex::Regex::new(r"^\s*(?:export\s+)?const\s+(\w+)\s*=").unwrap(), 13),
                (regex::Regex::new(r"^\s+(?:async\s+)?(\w+)\s*\([^)]*\)\s*\{").unwrap(), 6),
            ]
        }),
        "py" | "pyi" => PATTERNS.py.get_or_init(|| {
            vec![
                (regex::Regex::new(r"^\s*class\s+(\w+)").unwrap(), 5),
                (regex::Regex::new(r"^\s*def\s+(\w+)").unwrap(), 12),
            ]
        }),
        "go" => PATTERNS.go.get_or_init(|| {
            vec![
                (regex::Regex::new(r"^\s*func\s+(?:\([^)]*\)\s*)?(\w+)").unwrap(), 12),
                (regex::Regex::new(r"^\s*type\s+(\w+)\s+struct").unwrap(), 23),
            ]
        }),
        "rs" => PATTERNS.rs.get_or_init(|| {
            vec![
                (regex::Regex::new(r"^\s*(?:pub\s+)?fn\s+(\w+)").unwrap(), 12),
                (regex::Regex::new(r"^\s*(?:pub\s+)?struct\s+(\w+)").unwrap(), 23),
                (regex::Regex::new(r"^\s*(?:pub\s+)?enum\s+(\w+)").unwrap(), 10),
                (regex::Regex::new(r"^\s*(?:pub\s+)?trait\s+(\w+)").unwrap(), 11),
            ]
        }),
        "java" | "kt" => PATTERNS.java_kt.get_or_init(|| {
            vec![
                (regex::Regex::new(r"^\s*(?:public\s+|private\s+)?class\s+(\w+)").unwrap(), 5),
                (regex::Regex::new(r"^\s*(?:public\s+|private\s+)?(?:static\s+)?\w+\s+(\w+)\s*\([^)]*\)\s*\{").unwrap(), 6),
            ]
        }),
        _ => &[],
    }
}

/// Extract symbol-like definitions from a source file using per-extension
/// regex heuristics. This is intentionally simple (no real parser) — good
/// enough to build a searchable symbol index without an LSP server.
fn extract_symbols(path: &std::path::Path, content: &str) -> Vec<SymbolInformation> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let uri = format!("file://{}", path.display());
    let mut out = Vec::new();

    let patterns = patterns_for(ext);

    for (i, line) in content.lines().enumerate() {
        for (re, kind) in patterns {
            if let Some(caps) = re.captures(line) {
                if let Some(m) = caps.get(1) {
                    let name = m.as_str().to_string();
                    let col = m.start() as u32;
                    out.push(SymbolInformation {
                        name,
                        kind: *kind,
                        location: Location {
                            uri: uri.clone(),
                            range: Range {
                                start: Position { line: i as u32, character: col },
                                end: Position { line: i as u32, character: col + m.as_str().len() as u32 },
                            },
                        },
                        container_name: None,
                    });
                    break;
                }
            }
        }
    }

    out
}

const SOURCE_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "pyi", "go", "rs", "java", "kt",
];

impl Bm25Index {
    /// Build an index by walking the project root and extracting symbols from
    /// every recognized source file.
    pub fn build(project_root: &str) -> Self {
        let mut docs = Vec::new();
        for entry in WalkDir::new(project_root)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !(name.starts_with('.')
                    || name == "node_modules"
                    || name == "target"
                    || name == "dist"
                    || name == "build")
            })
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or("");
            if !SOURCE_EXTS.contains(&ext) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
            for sym in extract_symbols(entry.path(), &content) {
                let tokens = tokenize(&sym.name);
                docs.push(Doc { tokens, symbol: sym });
            }
        }
        Self::from_docs(docs)
    }

    pub fn from_docs(docs: Vec<Doc>) -> Self {
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        let mut total_len = 0usize;
        for d in &docs {
            total_len += d.tokens.len();
            let mut seen = std::collections::HashSet::new();
            for t in &d.tokens {
                if seen.insert(t.clone()) {
                    *doc_freq.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        let avg_len = if docs.is_empty() { 0.0 } else { total_len as f64 / docs.len() as f64 };
        Self { docs, doc_freq, avg_len }
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Score and rank all documents against a free-text query using Okapi BM25.
    pub fn search(&self, query: &str) -> Vec<(f64, &SymbolInformation)> {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() || self.docs.is_empty() {
            return vec![];
        }
        let n = self.docs.len() as f64;

        let mut scored: Vec<(f64, &SymbolInformation)> = self
            .docs
            .iter()
            .map(|doc| {
                let dl = doc.tokens.len() as f64;
                let mut score = 0.0;
                for qt in &q_tokens {
                    let tf = doc.tokens.iter().filter(|t| *t == qt).count() as f64;
                    // Prefix match bonus for partial identifier queries.
                    let tf = if tf == 0.0 && doc.tokens.iter().any(|t| t.starts_with(qt.as_str())) {
                        0.5
                    } else {
                        tf
                    };
                    if tf == 0.0 {
                        continue;
                    }
                    let df = *self.doc_freq.get(qt).unwrap_or(&0) as f64;
                    let df = if df == 0.0 { 0.5 } else { df };
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let denom = tf + K1 * (1.0 - B + B * dl / self.avg_len.max(1.0));
                    score += idf * (tf * (K1 + 1.0)) / denom;
                }
                (score, &doc.symbol)
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str) -> SymbolInformation {
        SymbolInformation {
            name: name.to_string(),
            kind: 12,
            location: Location {
                uri: "file:///a.rs".into(),
                range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
            },
            container_name: None,
        }
    }

    #[test]
    fn tokenizes_camel_case() {
        assert_eq!(tokenize("parseUserInput"), vec!["parse", "user", "input"]);
    }

    #[test]
    fn tokenizes_snake_case() {
        assert_eq!(tokenize("parse_user_input"), vec!["parse", "user", "input"]);
    }

    #[test]
    fn ranks_exact_match_above_unrelated() {
        let docs = vec![
            Doc { tokens: tokenize("computeTotal"), symbol: sym("computeTotal") },
            Doc { tokens: tokenize("renderWidget"), symbol: sym("renderWidget") },
        ];
        let idx = Bm25Index::from_docs(docs);
        let results = idx.search("compute total");
        assert!(!results.is_empty());
        assert_eq!(results[0].1.name, "computeTotal");
    }

    #[test]
    fn empty_query_returns_nothing() {
        let idx = Bm25Index::from_docs(vec![Doc { tokens: tokenize("foo"), symbol: sym("foo") }]);
        assert!(idx.search("").is_empty());
    }

    // --- extract_symbols: the per-language regex heuristics that determine
    // what BM25 search can find at all. Previously untested directly (only
    // exercised indirectly through LSP-gated integration tests), so a broken
    // pattern could regress silently.

    fn names(syms: &[SymbolInformation]) -> Vec<&str> {
        syms.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn extracts_typescript_class_interface_function_and_method() {
        let src = "export class Widget {\n  render() {}\n}\n\nexport interface Options {}\n\nexport function build() {}\n\nexport const CACHE = {};\n";
        let syms = extract_symbols(std::path::Path::new("widget.ts"), src);
        let found = names(&syms);
        assert!(found.contains(&"Widget"), "{found:?}");
        assert!(found.contains(&"Options"), "{found:?}");
        assert!(found.contains(&"build"), "{found:?}");
        assert!(found.contains(&"CACHE"), "{found:?}");
        assert!(found.contains(&"render"), "{found:?}");
    }

    #[test]
    fn extracts_python_class_and_function() {
        let src = "class User:\n    def greet(self):\n        pass\n\ndef create_user():\n    pass\n";
        let syms = extract_symbols(std::path::Path::new("user.py"), src);
        let found = names(&syms);
        assert!(found.contains(&"User"), "{found:?}");
        assert!(found.contains(&"greet"), "{found:?}");
        assert!(found.contains(&"create_user"), "{found:?}");
    }

    #[test]
    fn extracts_go_func_and_struct() {
        let src = "package main\n\ntype User struct {\n\tName string\n}\n\nfunc (u User) Greet() string {\n\treturn u.Name\n}\n\nfunc CreateUser() User {\n\treturn User{}\n}\n";
        let syms = extract_symbols(std::path::Path::new("user.go"), src);
        let found = names(&syms);
        assert!(found.contains(&"User"), "{found:?}");
        assert!(found.contains(&"CreateUser"), "{found:?}");
    }

    #[test]
    fn extracts_rust_fn_struct_enum_and_trait() {
        let src = "pub struct User {\n    name: String,\n}\n\npub enum Status {\n    Active,\n}\n\npub trait Greeter {}\n\npub fn create_user() -> User {\n    User { name: String::new() }\n}\n";
        let syms = extract_symbols(std::path::Path::new("user.rs"), src);
        let found = names(&syms);
        assert!(found.contains(&"User"), "{found:?}");
        assert!(found.contains(&"Status"), "{found:?}");
        assert!(found.contains(&"Greeter"), "{found:?}");
        assert!(found.contains(&"create_user"), "{found:?}");
    }

    #[test]
    fn extracts_java_class_and_method() {
        let src = "public class UserService {\n    public String greet() {\n        return \"hi\";\n    }\n}\n";
        let syms = extract_symbols(std::path::Path::new("UserService.java"), src);
        let found = names(&syms);
        assert!(found.contains(&"UserService"), "{found:?}");
        assert!(found.contains(&"greet"), "{found:?}");
    }

    #[test]
    fn unrecognized_extension_yields_no_symbols() {
        let syms = extract_symbols(std::path::Path::new("notes.md"), "# Heading\n\nclass NotReallyCode {}\n");
        assert!(syms.is_empty());
    }

    #[test]
    fn record_locations_use_the_provided_file_uri() {
        let syms = extract_symbols(std::path::Path::new("/abs/path/user.rs"), "pub struct User {}\n");
        assert_eq!(syms[0].location.uri, "file:///abs/path/user.rs");
    }
}
