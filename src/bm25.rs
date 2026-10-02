//! Self-contained BM25 scoring engine used as the fallback search path when no
//! LSP server workspace/symbol result is available (or no server is running
//! at all). Indexes symbols extracted with lightweight regex-based parsing
//! per language, then scores queries with the standard Okapi BM25 formula.

use std::collections::HashMap;
use std::sync::OnceLock;
use walkdir::WalkDir;

use crate::protocol::symbol_kind::{
    CLASS, ENUM, FIELD, FUNCTION, INTERFACE, KEY, METHOD, MODULE, STRUCT, TYPE_PARAMETER, VARIABLE,
};
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
    /// token -> indices of the documents containing it, ascending.
    ///
    /// Scoring used to walk every document for every query token, plus a
    /// second walk of that document's tokens for the prefix fallback
    /// whenever the exact count was zero — which is the common case. On a
    /// corpus of a few hundred thousand symbols that is the dominant cost
    /// of a search, and it does not get cheaper by caching the index.
    ///
    /// A postings list turns "which documents could possibly score above
    /// zero?" into a lookup. It is exactly equivalent, not an
    /// approximation: `search` discards any document scoring zero, and a
    /// document scores above zero only if some query token matches one of
    /// its tokens exactly or as a prefix — which is precisely the set this
    /// yields.
    postings: HashMap<String, Vec<u32>>,
    /// Every distinct token, sorted, so the prefix fallback can binary
    /// search for the range of tokens starting with a query token instead
    /// of testing all of them.
    sorted_tokens: Vec<String>,
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
    cpp: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    lua: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    zig: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    ruby: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    csharp: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    bash: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    css: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    json: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
    html: std::sync::OnceLock<Vec<(regex::Regex, u32)>>,
}

static PATTERNS: LangPatterns = LangPatterns {
    ts_js: std::sync::OnceLock::new(),
    py: std::sync::OnceLock::new(),
    go: std::sync::OnceLock::new(),
    rs: std::sync::OnceLock::new(),
    java_kt: std::sync::OnceLock::new(),
    cpp: std::sync::OnceLock::new(),
    lua: std::sync::OnceLock::new(),
    zig: std::sync::OnceLock::new(),
    ruby: std::sync::OnceLock::new(),
    csharp: std::sync::OnceLock::new(),
    bash: std::sync::OnceLock::new(),
    css: std::sync::OnceLock::new(),
    json: std::sync::OnceLock::new(),
    html: std::sync::OnceLock::new(),
};

fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).unwrap()
}

/// Control-flow keywords the method-shaped patterns can capture: `if (x) {`
/// looks exactly like a method `if()` to a pattern for TS/Java/C++/C#
/// method shorthand, and was indexed as one. Only words that are keywords
/// in every language this covers: `new`, `delete`, `match`, `select` and
/// the like are ordinary method names somewhere (`fn new`, Django's
/// `def delete`).
const NOT_A_NAME: &[&str] = &[
    "if", "else", "for", "foreach", "while", "switch", "catch", "return", "throw", "typeof",
    "sizeof",
];

fn patterns_for(ext: &str) -> &'static [(regex::Regex, u32)] {
    match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => PATTERNS.ts_js.get_or_init(|| {
            // `export`, `export default`, `declare`, `abstract` in front of
            // a declaration were mostly unmatched; so were `enum`, `type`
            // aliases, `let`/`var`, generator functions, and — the big one —
            // any method with a return type annotation (`greet(): string {`),
            // since the method pattern wanted `)` immediately before `{`.
            let lead = r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?";
            vec![
                (re(&format!(r"{lead}(?:abstract\s+)?class\s+(\w+)")), CLASS),
                (re(&format!(r"{lead}interface\s+(\w+)")), INTERFACE),
                (re(&format!(r"{lead}(?:const\s+)?enum\s+(\w+)")), ENUM),
                (re(&format!(r"{lead}type\s+(\w+)\s*(?:<[^>]*>)?\s*=")), TYPE_PARAMETER),
                (re(&format!(r"{lead}(?:async\s+)?function\s*\*?\s*(\w+)")), FUNCTION),
                (re(&format!(r"{lead}(?:const|let|var)\s+(\w+)\s*[:=]")), VARIABLE),
                (
                    re(r"^\s+(?:(?:public|private|protected|static|async|readonly|override|abstract|get|set)\s+)*\*?(\w+)\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{=;]+)?\{"),
                    METHOD,
                ),
            ]
        }),
        "py" | "pyi" => PATTERNS.py.get_or_init(|| {
            vec![
                (re(r"^\s*class\s+(\w+)"), CLASS),
                (re(r"^\s*(?:async\s+)?def\s+(\w+)"), FUNCTION),
            ]
        }),
        "go" => PATTERNS.go.get_or_init(|| {
            vec![
                (re(r"^\s*func\s+(?:\([^)]*\)\s*)?(\w+)"), FUNCTION),
                (re(r"^\s*type\s+(\w+)(?:\[[^\]]*\])?\s+struct\b"), STRUCT),
                (re(r"^\s*type\s+(\w+)(?:\[[^\]]*\])?\s+interface\b"), INTERFACE),
                (re(r"^\s*type\s+(\w+)\b"), TYPE_PARAMETER),
            ]
        }),
        "rs" => PATTERNS.rs.get_or_init(|| {
            // `pub(crate)`, `pub(super)`, and `const`/`async`/`unsafe`/
            // `extern "C"` before `fn` all used to hide a function.
            let vis = r"^\s*(?:pub(?:\([^)]*\))?\s+)?";
            vec![
                (
                    re(&format!(r#"{vis}(?:(?:const|async|unsafe|extern(?:\s+"[^"]*")?)\s+)*fn\s+(\w+)"#)),
                    FUNCTION,
                ),
                (re(&format!(r"{vis}struct\s+(\w+)")), STRUCT),
                (re(&format!(r"{vis}union\s+(\w+)")), STRUCT),
                (re(&format!(r"{vis}enum\s+(\w+)")), ENUM),
                (re(&format!(r"{vis}(?:unsafe\s+)?trait\s+(\w+)")), INTERFACE),
                (re(&format!(r"{vis}type\s+(\w+)")), TYPE_PARAMETER),
                (re(&format!(r"{vis}mod\s+(\w+)")), MODULE),
                (re(r"^\s*macro_rules!\s*(\w+)"), FUNCTION),
            ]
        }),
        "java" | "kt" => PATTERNS.java_kt.get_or_init(|| {
            let mods = r"^\s*(?:(?:public|private|protected|internal|abstract|final|static|sealed|open|data|inner|annotation)\s+)*";
            vec![
                // Before `class`, so Kotlin's `enum class` is an enum.
                (re(&format!(r"{mods}enum\s+(?:class\s+)?(\w+)")), ENUM),
                (re(&format!(r"{mods}(?:class|record|object)\s+(\w+)")), CLASS),
                (re(&format!(r"{mods}(?:interface|@interface)\s+(\w+)")), INTERFACE),
                // Kotlin: `fun name(`, `fun <T> name(`, `fun Type.name(`.
                (
                    re(&format!(r"{mods}(?:override\s+|suspend\s+|inline\s+|operator\s+)*fun\s+(?:<[^>]*>\s*)?(?:[\w.<>, ?*]+\.)?(\w+)\s*\(")),
                    METHOD,
                ),
                // Java: modifiers, a return type (generic, array), the name.
                (
                    re(&format!(r"{mods}(?:synchronized\s+|native\s+|default\s+)*(?:<[^>]*>\s+)?[\w.<>\[\],? ]+\s+(\w+)\s*\([^)]*\)\s*(?:throws\s+[\w.,\s]+)?\{{")),
                    METHOD,
                ),
            ]
        }),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => PATTERNS.cpp.get_or_init(|| {
            vec![
                (
                    regex::Regex::new(r"^\s*(?:typedef\s+)?(?:struct|class)\s+(\w+)").unwrap(),
                    CLASS,
                ),
                (regex::Regex::new(r"^\s*namespace\s+(\w+)").unwrap(), MODULE),
                (
                    // Loosely matches a function definition (return type +
                    // name + parens + opening brace on the same line) —
                    // same tradeoff as the existing Java/C# heuristics:
                    // misses multi-line signatures, doesn't try to exclude
                    // control-flow keywords that share the shape.
                    regex::Regex::new(r"^\s*(?:static\s+|inline\s+|virtual\s+)?[\w:<>\*&]+\s+(\w+)\s*\([^;]*\)\s*\{").unwrap(),
                    FUNCTION,
                ),
            ]
        }),
        "lua" => PATTERNS.lua.get_or_init(|| {
            vec![
                (
                    regex::Regex::new(r"^\s*local\s+function\s+(\w+)").unwrap(),
                    FUNCTION,
                ),
                (
                    regex::Regex::new(r"^\s*function\s+(?:[\w.:]+[.:])?(\w+)\s*\(").unwrap(),
                    FUNCTION,
                ),
            ]
        }),
        "zig" => PATTERNS.zig.get_or_init(|| {
            vec![
                (
                    regex::Regex::new(r"^\s*(?:pub\s+)?fn\s+(\w+)").unwrap(),
                    FUNCTION,
                ),
                (
                    regex::Regex::new(r"^\s*(?:pub\s+)?const\s+(\w+)\s*=\s*struct").unwrap(),
                    STRUCT,
                ),
                (
                    regex::Regex::new(r"^\s*(?:pub\s+)?const\s+(\w+)\s*=\s*enum").unwrap(),
                    ENUM,
                ),
            ]
        }),
        "rb" => PATTERNS.ruby.get_or_init(|| {
            vec![
                (regex::Regex::new(r"^\s*class\s+(\w+)").unwrap(), CLASS),
                (regex::Regex::new(r"^\s*module\s+(\w+)").unwrap(), MODULE),
                (
                    regex::Regex::new(r"^\s*def\s+(?:self\.)?(\w+)").unwrap(),
                    FUNCTION,
                ),
            ]
        }),
        "cs" => PATTERNS.csharp.get_or_init(|| {
            let mods = r"^\s*(?:(?:public|private|protected|internal|abstract|sealed|static|partial|readonly|virtual|override|async|unsafe|new|extern)\s+)*";
            vec![
                (re(&format!(r"{mods}(?:class|record)\s+(\w+)")), CLASS),
                (re(&format!(r"{mods}struct\s+(\w+)")), STRUCT),
                (re(&format!(r"{mods}interface\s+(\w+)")), INTERFACE),
                (re(&format!(r"{mods}enum\s+(\w+)")), ENUM),
                (
                    re(&format!(r"{mods}[\w.<>\[\],? ]+\s+(\w+)\s*(?:<[^>]*>)?\([^)]*\)\s*(?:where\s+[^{{]+)?\{{")),
                    METHOD,
                ),
            ]
        }),
        "sh" | "bash" => PATTERNS.bash.get_or_init(|| {
            vec![
                (
                    regex::Regex::new(r"^\s*function\s+(\w+)").unwrap(),
                    FUNCTION,
                ),
                (
                    regex::Regex::new(r"^\s*(\w+)\s*\(\)\s*\{?").unwrap(),
                    FUNCTION,
                ),
            ]
        }),
        "css" | "scss" | "less" => PATTERNS.css.get_or_init(|| {
            vec![
                (
                    regex::Regex::new(r"^\s*\.([\w-]+)\s*[,{]").unwrap(),
                    CLASS,
                ),
                (regex::Regex::new(r"^\s*#([\w-]+)\s*[,{]").unwrap(), FIELD),
            ]
        }),
        "json" | "jsonc" => PATTERNS.json.get_or_init(|| {
            vec![(
                regex::Regex::new(r#"^\s*"([\w\-.]+)"\s*:"#).unwrap(),
                KEY,
            )]
        }),
        "html" | "htm" => PATTERNS.html.get_or_init(|| {
            vec![(
                regex::Regex::new(r#"\bid\s*=\s*["']([\w-]+)["']"#).unwrap(),
                FIELD,
            )]
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
                    // `return new Runnable() {` is an expression, not a
                    // method called `Runnable`.
                    if NOT_A_NAME.contains(&m.as_str())
                        || line[..m.start()].trim_end().ends_with("new")
                    {
                        continue;
                    }
                    let name = m.as_str().to_string();
                    let col = m.start() as u32;
                    out.push(SymbolInformation {
                        name,
                        kind: *kind,
                        location: Location {
                            uri: uri.clone(),
                            range: Range {
                                start: Position {
                                    line: i as u32,
                                    character: col,
                                },
                                end: Position {
                                    line: i as u32,
                                    character: col + m.as_str().len() as u32,
                                },
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

/// Directory names never worth indexing or walking into: dependency trees,
/// build output, and VCS metadata. Shared with `commands.rs::try_lsp_search`
/// so the two walks can't drift apart.
///
/// Callers must exempt the walk root (`depth() == 0`) — see `Bm25Index::build`.
pub fn is_ignored_dir_name(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | "dist" | "build")
}

/// Extensions the fallback index reads. Derived from the registry rather
/// than hand-listed: this was previously a literal that had already drifted,
/// missing `.mts`/`.cts` (typescript) and `.kts` (kotlin), so `lsp search`
/// silently indexed nothing from those files whenever it fell back here.
fn source_exts() -> &'static std::collections::HashSet<String> {
    static EXTS: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    EXTS.get_or_init(|| {
        crate::registry::languages()
            .iter()
            .flat_map(|l| l.extensions.iter())
            .map(|e| e.trim_start_matches('.').to_string())
            .collect()
    })
}

/// Cheap summary of the source files under a project root, used to decide
/// whether a cached index is still current.
///
/// Deliberately stat-only: no file is opened and no regex runs, so
/// computing this is a couple of orders of magnitude cheaper than
/// rebuilding the index. Any edit changes the newest mtime; adding or
/// removing a file changes the count; a same-mtime edit that preserved the
/// count would still have to preserve the total byte size too, which does
/// not happen in practice on a filesystem with nanosecond mtimes.
///
/// This is used instead of the file watcher because the watcher only
/// watches roots that have a live language server, and only for that one
/// language's extensions — it cannot see the whole set of files this index
/// spans, so it would miss changes and serve stale results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFingerprint {
    files: usize,
    /// Order-independent combination of every file's (path, length, mtime).
    ///
    /// This used to be a total length plus the newest mtime, which a rename
    /// leaves unchanged (`mv` keeps the size and the mtime), so the cached
    /// index kept returning the old path long after the file had moved. It
    /// also missed an edit that kept a file's size while restoring an older
    /// mtime. Same stat walk, same cost.
    digest: u64,
}

impl TreeFingerprint {
    pub fn of(project_root: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut files = 0usize;
        let mut digest = 0u64;
        for entry in source_files(project_root) {
            let Ok(meta) = entry.metadata() else { continue };
            files += 1;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            entry.path().hash(&mut h);
            meta.len().hash(&mut h);
            meta.modified().ok().hash(&mut h);
            // Wrapping sum: independent of walk order, and unlike XOR two
            // identical entries don't cancel out.
            digest = digest.wrapping_add(h.finish());
        }
        Self { files, digest }
    }
}

/// Every indexable source file under `project_root`, with the shared
/// ignore list applied.
fn source_files(project_root: &str) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(project_root)
        .into_iter()
        // `depth() == 0` is the project root itself. walkdir applies this
        // predicate to the root too and calls `skip_current_dir()` when it
        // fails, which ends the walk immediately — so indexing a project
        // that simply *lives* in a dotfile directory (`~/.dotfiles`,
        // `~/.config/nvim`) produced an empty index and a confident
        // "No matches found."
        .filter_entry(|e| e.depth() == 0 || !is_ignored_dir_name(&e.file_name().to_string_lossy()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let ext = e.path().extension().and_then(|x| x.to_str()).unwrap_or("");
            source_exts().contains(&ext.to_ascii_lowercase())
        })
}

impl Bm25Index {
    /// Build an index by walking the project root and extracting symbols from
    /// every recognized source file.
    pub fn build(project_root: &str) -> Self {
        let mut docs = Vec::new();
        for entry in source_files(project_root) {
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for sym in extract_symbols(entry.path(), &content) {
                let tokens = tokenize(&sym.name);
                docs.push(Doc {
                    tokens,
                    symbol: sym,
                });
            }
        }
        Self::from_docs(docs)
    }

    pub fn from_docs(docs: Vec<Doc>) -> Self {
        let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
        let mut total_len = 0usize;
        for (i, d) in docs.iter().enumerate() {
            total_len += d.tokens.len();
            for t in &d.tokens {
                // Tokens repeat within a document (a symbol named
                // `user_user` tokenizes to two `user`s); the postings list
                // holds each document once, which is also what makes its
                // length the document frequency. Documents are appended in
                // ascending order, so checking the tail is enough to
                // deduplicate, and the list stays sorted for free.
                let entry = match postings.get_mut(t.as_str()) {
                    Some(e) => e,
                    None => postings.entry(t.clone()).or_default(),
                };
                if entry.last() != Some(&(i as u32)) {
                    entry.push(i as u32);
                }
            }
        }
        let mut sorted_tokens: Vec<String> = postings.keys().cloned().collect();
        sorted_tokens.sort_unstable();

        let avg_len = if docs.is_empty() {
            0.0
        } else {
            total_len as f64 / docs.len() as f64
        };
        Self {
            docs,
            postings,
            sorted_tokens,
            avg_len,
        }
    }

    /// Document frequency: how many documents contain `token`.
    ///
    /// Derived from the postings list rather than kept in a second map, so
    /// there is one source of truth and one allocation per distinct token
    /// instead of the two `String` clones per token occurrence this used to
    /// do while building a parallel `doc_freq`.
    fn doc_freq(&self, token: &str) -> usize {
        self.postings.get(token).map_or(0, |p| p.len())
    }

    /// Distinct tokens beginning with `prefix`.
    fn tokens_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        let start = self.sorted_tokens.partition_point(|t| t.as_str() < prefix);
        self.sorted_tokens[start..]
            .iter()
            .take_while(move |t| t.starts_with(prefix))
    }

    /// Documents that could score above zero for `q_tokens`.
    fn candidates(&self, q_tokens: &[String]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for qt in q_tokens {
            // `tokens_with_prefix` already includes an exact match, since
            // a token is a prefix of itself.
            for tok in self.tokens_with_prefix(qt) {
                if let Some(p) = self.postings.get(tok) {
                    out.extend_from_slice(p);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Score and rank all documents against a free-text query using Okapi BM25.
    pub fn search(&self, query: &str) -> Vec<(f64, &SymbolInformation)> {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() || self.docs.is_empty() {
            return vec![];
        }
        let n = self.docs.len() as f64;

        let mut scored: Vec<(f64, &SymbolInformation)> = self
            .candidates(&q_tokens)
            .into_iter()
            .map(|i| &self.docs[i as usize])
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
                    let df = self.doc_freq(qt) as f64;
                    let df = if df == 0.0 { 0.5 } else { df };
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let denom = tf + K1 * (1.0 - B + B * dl / self.avg_len.max(1.0));
                    score += idf * (tf * (K1 + 1.0)) / denom;
                }
                (score, &doc.symbol)
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();

        scored.sort_by(rank_order);
        scored
    }
}

/// Highest score first; ties broken by location and name. Without the
/// tiebreak, equal scores kept the directory walk's order, which can
/// change between index builds — so the same query could page differently
/// before and after an unrelated rebuild.
fn rank_order(a: &(f64, &SymbolInformation), b: &(f64, &SymbolInformation)) -> std::cmp::Ordering {
    b.0.partial_cmp(&a.0)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.1.location.uri.cmp(&b.1.location.uri))
        .then_with(|| {
            a.1.location
                .range
                .start
                .line
                .cmp(&b.1.location.range.start.line)
        })
        .then_with(|| a.1.name.cmp(&b.1.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force reference implementation: scores every document, exactly
    /// as `search` did before the postings list was introduced.
    ///
    /// The postings list is only a way to skip documents that cannot score
    /// above zero, so the two must agree on every query. This pins that.
    fn brute_force_search<'a>(
        index: &'a Bm25Index,
        query: &str,
    ) -> Vec<(f64, &'a SymbolInformation)> {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() || index.docs.is_empty() {
            return vec![];
        }
        let n = index.docs.len() as f64;
        let mut scored: Vec<(f64, &SymbolInformation)> = index
            .docs
            .iter()
            .map(|doc| {
                let dl = doc.tokens.len() as f64;
                let mut score = 0.0;
                for qt in &q_tokens {
                    let tf = doc.tokens.iter().filter(|t| *t == qt).count() as f64;
                    let tf = if tf == 0.0 && doc.tokens.iter().any(|t| t.starts_with(qt.as_str())) {
                        0.5
                    } else {
                        tf
                    };
                    if tf == 0.0 {
                        continue;
                    }
                    let df = index.doc_freq(qt) as f64;
                    let df = if df == 0.0 { 0.5 } else { df };
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let denom = tf + K1 * (1.0 - B + B * dl / index.avg_len.max(1.0));
                    score += idf * (tf * (K1 + 1.0)) / denom;
                }
                (score, &doc.symbol)
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();
        scored.sort_by(rank_order);
        scored
    }

    fn corpus() -> Bm25Index {
        let names = [
            "UserService",
            "createUser",
            "user_repository",
            "UserServiceImpl",
            "AccountService",
            "create",
            "userId",
            "deleteAccount",
            "user_user_helper",
            "HTTPServer",
            "parseJSON",
            "unrelated",
        ];
        Bm25Index::from_docs(
            names
                .iter()
                .map(|n| Doc {
                    tokens: tokenize(n),
                    symbol: sym(n),
                })
                .collect(),
        )
    }

    #[test]
    fn postings_search_matches_a_brute_force_scan() {
        let index = corpus();
        for query in [
            "user",
            "User",
            "createUser",
            "serv",
            "us",
            "account",
            "http",
            "json",
            "nothing_matches_this",
            "user service",
            "u",
        ] {
            let fast = index.search(query);
            let slow = brute_force_search(&index, query);
            assert_eq!(
                fast.len(),
                slow.len(),
                "different result count for {query:?}"
            );
            for (a, b) in fast.iter().zip(slow.iter()) {
                assert_eq!(a.1.name, b.1.name, "different ranking for {query:?}");
                assert!(
                    (a.0 - b.0).abs() < f64::EPSILON,
                    "different score for {query:?}: {} vs {}",
                    a.0,
                    b.0
                );
            }
        }
    }

    #[test]
    fn document_frequency_counts_documents_not_occurrences() {
        // `user_user_helper` tokenizes to two `user`s but must count once,
        // or the idf term is wrong and the postings list would carry a
        // duplicate entry.
        let index = corpus();
        let with_user = [
            "UserService",
            "createUser",
            "user_repository",
            "UserServiceImpl",
            "userId",
            "user_user_helper",
        ];
        assert_eq!(index.doc_freq("user"), with_user.len());
        assert_eq!(index.postings.get("user").unwrap().len(), with_user.len());
    }

    #[test]
    fn postings_lists_are_sorted_and_deduplicated() {
        let index = corpus();
        for (token, docs) in &index.postings {
            let mut sorted = docs.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(*docs, sorted, "postings for {token:?} not sorted/deduped");
        }
    }

    #[test]
    fn prefix_lookup_finds_exactly_the_tokens_with_that_prefix() {
        let index = corpus();
        let found: Vec<&str> = index.tokens_with_prefix("us").map(|t| t.as_str()).collect();
        let expected: Vec<&str> = index
            .sorted_tokens
            .iter()
            .filter(|t| t.starts_with("us"))
            .map(|t| t.as_str())
            .collect();
        assert_eq!(found, expected);
        assert!(found.contains(&"user"));
        assert!(!found.contains(&"account"));
    }

    #[test]
    fn a_query_matching_nothing_returns_nothing() {
        assert!(corpus().search("zzzznomatch").is_empty());
    }

    fn sym(name: &str) -> SymbolInformation {
        SymbolInformation {
            name: name.to_string(),
            kind: 12,
            location: Location {
                uri: "file:///a.rs".into(),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
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
            Doc {
                tokens: tokenize("computeTotal"),
                symbol: sym("computeTotal"),
            },
            Doc {
                tokens: tokenize("renderWidget"),
                symbol: sym("renderWidget"),
            },
        ];
        let idx = Bm25Index::from_docs(docs);
        let results = idx.search("compute total");
        assert!(!results.is_empty());
        assert_eq!(results[0].1.name, "computeTotal");
    }

    #[test]
    fn empty_query_returns_nothing() {
        let idx = Bm25Index::from_docs(vec![Doc {
            tokens: tokenize("foo"),
            symbol: sym("foo"),
        }]);
        assert!(idx.search("").is_empty());
    }

    // --- extract_symbols: the per-language regex heuristics that determine
    // what BM25 search can find at all. Previously untested directly (only
    // exercised indirectly through LSP-gated integration tests), so a broken
    // pattern could regress silently.

    fn names(syms: &[SymbolInformation]) -> Vec<&str> {
        syms.iter().map(|s| s.name.as_str()).collect()
    }

    /// `(name, kind)` for every symbol extracted from `src` as `file`.
    fn extracted(file: &str, src: &str) -> Vec<(String, &'static str)> {
        extract_symbols(std::path::Path::new(file), src)
            .iter()
            .map(|s| (s.name.clone(), crate::protocol::symbol_kind_name(s.kind)))
            .collect()
    }

    fn pairs(expected: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
        expected.iter().map(|(n, k)| (n.to_string(), *k)).collect()
    }

    /// Exactly what is found, per language, in declaration forms that real
    /// code uses all the time and the patterns used to miss — and nothing
    /// that isn't a declaration (`if (x) {` was indexed as a method `if`).
    #[test]
    fn typescript_declarations_are_all_found_and_control_flow_is_not() {
        let src = "\
export default class App {}
export abstract class Shape {
  abstract area(): number;
  private static async load<T>(id: string): Promise<T> {
    if (id) {
      for (const x of []) {}
    }
  }
  get size(): number {
    return 1;
  }
}
export const enum Color { Red }
export type UserId = string;
declare interface Window {}
export async function* stream() {}
let counter: number = 0;
";
        assert_eq!(
            extracted("a.ts", src),
            pairs(&[
                ("App", "class"),
                ("Shape", "class"),
                ("load", "method"),
                ("size", "method"),
                ("Color", "enum"),
                ("UserId", "typeParameter"),
                ("Window", "interface"),
                ("stream", "function"),
                ("counter", "variable"),
            ])
        );
    }

    #[test]
    fn ordinary_names_that_are_keywords_elsewhere_are_still_found() {
        let rs = "impl Foo {\n    pub fn new() -> Self { Foo }\n    fn lock(&self) {}\n    fn select(&self) {}\n}\n";
        let found = extracted("a.rs", rs);
        for name in ["new", "lock", "select"] {
            assert!(
                found.contains(&(name.into(), "function")),
                "{name}: {found:?}"
            );
        }
        let py = "class Model:\n    def delete(self):\n        pass\n    def using(self, db):\n        pass\n";
        let found = extracted("a.py", py);
        assert!(found.contains(&("delete".into(), "function")), "{found:?}");
        assert!(found.contains(&("using".into(), "function")), "{found:?}");
        let ts = "class Repo {\n  async delete(id: string): Promise<void> {\n  }\n}\n";
        assert!(extracted("a.ts", ts).contains(&("delete".into(), "method")));
    }

    #[test]
    fn an_anonymous_class_expression_is_not_a_method() {
        let java = "class A {\n    Runnable r() {\n        return new Runnable() {\n        };\n    }\n}\n";
        let found = extracted("A.java", java);
        assert!(!found.iter().any(|(n, _)| n == "Runnable"), "{found:?}");
        assert!(found.contains(&("r".into(), "method")), "{found:?}");
    }

    #[test]
    fn python_async_functions_are_found() {
        let src = "class Repo:\n    async def fetch_items(self):\n        pass\n\nasync def main():\n    pass\n";
        assert_eq!(
            extracted("a.py", src),
            pairs(&[
                ("Repo", "class"),
                ("fetch_items", "function"),
                ("main", "function")
            ])
        );
    }

    #[test]
    fn go_interfaces_and_other_types_are_found() {
        let src = "type Greeter interface {\n\tGreet() string\n}\ntype ID string\ntype Box[T any] struct{}\nfunc (b *Box[T]) Get() T {}\n";
        assert_eq!(
            extracted("a.go", src),
            pairs(&[
                ("Greeter", "interface"),
                ("ID", "typeParameter"),
                ("Box", "struct"),
                ("Get", "function"),
            ])
        );
    }

    #[test]
    fn rust_functions_behind_qualifiers_are_found() {
        let src = "\
pub(crate) async fn resync_from_disk() {}
pub const fn zero() -> u32 { 0 }
unsafe fn raw() {}
pub(super) struct Cache;
pub type Result<T> = std::result::Result<T, Error>;
mod tests {}
pub unsafe trait Send2 {}
macro_rules! bail2 { () => {} }
impl Cache {}
";
        assert_eq!(
            extracted("a.rs", src),
            pairs(&[
                ("resync_from_disk", "function"),
                ("zero", "function"),
                ("raw", "function"),
                ("Cache", "struct"),
                ("Result", "typeParameter"),
                ("tests", "module"),
                ("Send2", "interface"),
                ("bail2", "function"),
            ])
        );
    }

    #[test]
    fn java_kotlin_and_csharp_declarations_are_found_and_control_flow_is_not() {
        let java = "public final class A {\n    public static List<String> names(int n) throws IOException {\n        if (n > 0) {\n        }\n    }\n}\npublic enum Mode { ON }\npublic record Point(int x, int y) {}\n";
        assert_eq!(
            extracted("A.java", java),
            pairs(&[
                ("A", "class"),
                ("names", "method"),
                ("Mode", "enum"),
                ("Point", "class")
            ])
        );
        let kotlin = "data class User(val name: String)\nobject Registry\nsealed interface Shape\nenum class Color { RED }\nfun <T> List<T>.second(): T = this[1]\nsuspend fun load() {}\n";
        assert_eq!(
            extracted("a.kt", kotlin),
            pairs(&[
                ("User", "class"),
                ("Registry", "class"),
                ("Shape", "interface"),
                ("Color", "enum"),
                ("second", "method"),
                ("load", "method"),
            ])
        );
        let cs = "public sealed partial class Svc\n{\n    public async Task<List<int>> LoadAsync(int id)\n    {\n        while (true) {\n        }\n    }\n}\npublic readonly struct P {}\npublic record Rec(int X);\n";
        let found = extracted("a.cs", cs);
        assert!(found.contains(&("Svc".into(), "class")), "{found:?}");
        assert!(found.contains(&("P".into(), "struct")), "{found:?}");
        assert!(found.contains(&("Rec".into(), "class")), "{found:?}");
        assert!(!found.iter().any(|(n, _)| n == "while"), "{found:?}");
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
        let src =
            "class User:\n    def greet(self):\n        pass\n\ndef create_user():\n    pass\n";
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
    fn extracts_cpp_struct_namespace_and_function() {
        let src = "namespace app {\n\nstruct User {\n    std::string name;\n};\n\nint add(int a, int b) {\n    return a + b;\n}\n\n}\n";
        let syms = extract_symbols(std::path::Path::new("user.cpp"), src);
        let found = names(&syms);
        assert!(found.contains(&"app"), "{found:?}");
        assert!(found.contains(&"User"), "{found:?}");
        assert!(found.contains(&"add"), "{found:?}");
    }

    #[test]
    fn extracts_lua_local_and_global_function() {
        let src = "local function add(a, b)\n    return a + b\nend\n\nfunction Greeter.greet(self)\n    return \"hi\"\nend\n";
        let syms = extract_symbols(std::path::Path::new("main.lua"), src);
        let found = names(&syms);
        assert!(found.contains(&"add"), "{found:?}");
        assert!(found.contains(&"greet"), "{found:?}");
    }

    #[test]
    fn extracts_zig_fn_struct_and_enum() {
        let src = "pub fn main() void {}\n\nconst User = struct {\n    name: []const u8,\n};\n\nconst Status = enum {\n    active,\n};\n";
        let syms = extract_symbols(std::path::Path::new("main.zig"), src);
        let found = names(&syms);
        assert!(found.contains(&"main"), "{found:?}");
        assert!(found.contains(&"User"), "{found:?}");
        assert!(found.contains(&"Status"), "{found:?}");
    }

    #[test]
    fn extracts_ruby_class_module_and_def() {
        let src = "module App\n  class Greeter\n    def greet(name)\n      \"hi #{name}\"\n    end\n  end\nend\n";
        let syms = extract_symbols(std::path::Path::new("greeter.rb"), src);
        let found = names(&syms);
        assert!(found.contains(&"App"), "{found:?}");
        assert!(found.contains(&"Greeter"), "{found:?}");
        assert!(found.contains(&"greet"), "{found:?}");
    }

    #[test]
    fn extracts_csharp_class_interface_and_method() {
        let src = "namespace App;\n\npublic interface IGreeter {}\n\npublic class Greeter : IGreeter {\n    public string Greet() {\n        return \"hi\";\n    }\n}\n";
        let syms = extract_symbols(std::path::Path::new("Greeter.cs"), src);
        let found = names(&syms);
        assert!(found.contains(&"IGreeter"), "{found:?}");
        assert!(found.contains(&"Greeter"), "{found:?}");
        assert!(found.contains(&"Greet"), "{found:?}");
    }

    #[test]
    fn extracts_bash_function() {
        let src = "function greet() {\n    echo hello\n}\n\nother_task() {\n    echo bye\n}\n";
        let syms = extract_symbols(std::path::Path::new("main.sh"), src);
        let found = names(&syms);
        assert!(found.contains(&"greet"), "{found:?}");
        assert!(found.contains(&"other_task"), "{found:?}");
    }

    #[test]
    fn extracts_css_class_and_id_selectors() {
        let src = ".card {\n  color: red;\n}\n\n#header {\n  color: blue;\n}\n";
        let syms = extract_symbols(std::path::Path::new("style.css"), src);
        let found = names(&syms);
        assert!(found.contains(&"card"), "{found:?}");
        assert!(found.contains(&"header"), "{found:?}");
    }

    #[test]
    fn extracts_json_top_level_keys() {
        let src = "{\n  \"name\": \"lsp-cli\",\n  \"version\": \"0.1.0\"\n}\n";
        let syms = extract_symbols(std::path::Path::new("package.json"), src);
        let found = names(&syms);
        assert!(found.contains(&"name"), "{found:?}");
        assert!(found.contains(&"version"), "{found:?}");
    }

    #[test]
    fn extracts_html_element_ids() {
        let src = "<div id=\"app\">\n  <span id=\"greeting\">hi</span>\n</div>\n";
        let syms = extract_symbols(std::path::Path::new("index.html"), src);
        let found = names(&syms);
        assert!(found.contains(&"app"), "{found:?}");
        assert!(found.contains(&"greeting"), "{found:?}");
    }

    #[test]
    fn unrecognized_extension_yields_no_symbols() {
        let syms = extract_symbols(
            std::path::Path::new("notes.md"),
            "# Heading\n\nclass NotReallyCode {}\n",
        );
        assert!(syms.is_empty());
    }

    #[test]
    fn record_locations_use_the_provided_file_uri() {
        let syms = extract_symbols(
            std::path::Path::new("/abs/path/user.rs"),
            "pub struct User {}\n",
        );
        assert_eq!(syms[0].location.uri, "file:///abs/path/user.rs");
    }

    // --- TreeFingerprint ----------------------------------------------

    #[test]
    fn fingerprint_changes_when_a_file_is_renamed() {
        // `mv` keeps the size and the mtime, so the old count + total size
        // + newest mtime fingerprint stayed equal and the cache kept
        // returning a path that no longer existed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("one.lua"), "function a() end\n").unwrap();
        let before = TreeFingerprint::of(root);
        std::fs::rename(dir.path().join("one.lua"), dir.path().join("two.lua")).unwrap();
        assert_ne!(before, TreeFingerprint::of(root));
    }

    #[test]
    fn fingerprint_changes_when_an_older_same_size_version_is_restored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let f = dir.path().join("a.lua");
        std::fs::write(&f, "function a() end\n").unwrap();
        let old_mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let before = TreeFingerprint::of(root);
        std::fs::write(&f, "function b() end\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old_mtime - std::time::Duration::from_secs(60))
            .unwrap();
        assert_ne!(before, TreeFingerprint::of(root));
    }
    // The cached index in the daemon is only correct if this notices every
    // change that could alter the index.

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn fingerprint_is_stable_when_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "pub fn one() {}");
        let root = dir.path().to_str().unwrap();
        assert_eq!(TreeFingerprint::of(root), TreeFingerprint::of(root));
    }

    #[test]
    fn fingerprint_changes_when_a_file_is_added() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "pub fn one() {}");
        let root = dir.path().to_str().unwrap();
        let before = TreeFingerprint::of(root);
        write(dir.path(), "b.rs", "pub fn two() {}");
        assert_ne!(before, TreeFingerprint::of(root));
    }

    #[test]
    fn fingerprint_changes_when_a_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "pub fn one() {}");
        write(dir.path(), "b.rs", "pub fn two() {}");
        let root = dir.path().to_str().unwrap();
        let before = TreeFingerprint::of(root);
        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        assert_ne!(before, TreeFingerprint::of(root));
    }

    #[test]
    fn fingerprint_changes_when_a_file_is_edited() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "pub fn one() {}");
        let root = dir.path().to_str().unwrap();
        let before = TreeFingerprint::of(root);
        // A different length, so this holds even where mtime resolution is
        // coarse.
        write(dir.path(), "a.rs", "pub fn one_renamed_longer() {}");
        assert_ne!(before, TreeFingerprint::of(root));
    }

    #[test]
    fn fingerprint_ignores_files_the_index_does_not_read() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "pub fn one() {}");
        let root = dir.path().to_str().unwrap();
        let before = TreeFingerprint::of(root);
        // Not a source extension, and inside an ignored directory: neither
        // can affect the index, so neither should force a rebuild.
        write(dir.path(), "notes.txt", "hello");
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        write(
            &dir.path().join("node_modules"),
            "dep.rs",
            "pub fn dep() {}",
        );
        assert_eq!(before, TreeFingerprint::of(root));
    }

    #[test]
    fn a_project_root_that_is_a_dot_directory_is_still_fingerprinted() {
        // Same trap as the index walk: walkdir applies filter_entry to the
        // root, so a root named `.config` would otherwise prune itself and
        // fingerprint as empty, making every rebuild look unnecessary.
        let parent = tempfile::tempdir().unwrap();
        let root_dir = parent.path().join(".dotfiles");
        std::fs::create_dir_all(&root_dir).unwrap();
        write(&root_dir, "a.rs", "pub fn one() {}");
        let fp = TreeFingerprint::of(root_dir.to_str().unwrap());
        assert_ne!(
            fp,
            TreeFingerprint::of(parent.path().join("empty").to_str().unwrap())
        );
    }
}
