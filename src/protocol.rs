use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// The `LocationLink` shape a server may return instead of plain
/// `Location`s when the client capability declares `linkSupport` (or, as
/// observed live with deno's LSP, even when it doesn't explicitly — deno
/// sends `LocationLink[]` for `textDocument/definition` unconditionally).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationLink {
    #[serde(rename = "targetUri")]
    pub target_uri: String,
    #[serde(rename = "targetSelectionRange")]
    pub target_selection_range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LocationOrMany {
    One(Location),
    Many(Vec<Location>),
    Links(Vec<LocationLink>),
}

impl LocationOrMany {
    pub fn into_vec(self) -> Vec<Location> {
        match self {
            LocationOrMany::One(l) => vec![l],
            LocationOrMany::Many(v) => v,
            LocationOrMany::Links(links) => links
                .into_iter()
                .map(|l| Location {
                    uri: l.target_uri,
                    range: l.target_selection_range,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbol {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub kind: u32,
    pub range: Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DocumentSymbol>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInformation {
    pub name: String,
    pub kind: u32,
    pub location: Location,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "containerName"
    )]
    pub container_name: Option<String>,
}

/// LSP `SymbolKind` numeric values (spec-defined, 1-indexed). Named so call
/// sites filtering/rendering by kind (`commands.rs::filter_top_level`,
/// `format.rs::icon`) don't re-derive meaning from bare integers.
#[allow(dead_code)]
pub mod symbol_kind {
    pub const FILE: u32 = 1;
    pub const MODULE: u32 = 2;
    pub const NAMESPACE: u32 = 3;
    pub const PACKAGE: u32 = 4;
    pub const CLASS: u32 = 5;
    pub const METHOD: u32 = 6;
    pub const PROPERTY: u32 = 7;
    pub const FIELD: u32 = 8;
    pub const CONSTRUCTOR: u32 = 9;
    pub const ENUM: u32 = 10;
    pub const INTERFACE: u32 = 11;
    pub const FUNCTION: u32 = 12;
    pub const VARIABLE: u32 = 13;
    pub const CONSTANT: u32 = 14;
    pub const STRING: u32 = 15;
    pub const NUMBER: u32 = 16;
    pub const BOOLEAN: u32 = 17;
    pub const ARRAY: u32 = 18;
    pub const OBJECT: u32 = 19;
    pub const KEY: u32 = 20;
    pub const NULL: u32 = 21;
    pub const ENUM_MEMBER: u32 = 22;
    pub const STRUCT: u32 = 23;
    pub const EVENT: u32 = 24;
    pub const OPERATOR: u32 = 25;
    pub const TYPE_PARAMETER: u32 = 26;
}

pub fn symbol_kind_name(kind: u32) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enumMember",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "typeParameter",
        _ => "unknown",
    }
}

/// Every `SymbolKind` value the LSP spec defines, in numeric order.
///
/// Replaces a bare `(1u32..=26)` range in `commands.rs` and the unused
/// `TOP_LEVEL_KINDS` constant that duplicated `commands.rs::filter_top_level`
/// in magic numbers.
pub const ALL_SYMBOL_KIND_IDS: &[u32] = &[
    symbol_kind::FILE,
    symbol_kind::MODULE,
    symbol_kind::NAMESPACE,
    symbol_kind::PACKAGE,
    symbol_kind::CLASS,
    symbol_kind::METHOD,
    symbol_kind::PROPERTY,
    symbol_kind::FIELD,
    symbol_kind::CONSTRUCTOR,
    symbol_kind::ENUM,
    symbol_kind::INTERFACE,
    symbol_kind::FUNCTION,
    symbol_kind::VARIABLE,
    symbol_kind::CONSTANT,
    symbol_kind::STRING,
    symbol_kind::NUMBER,
    symbol_kind::BOOLEAN,
    symbol_kind::ARRAY,
    symbol_kind::OBJECT,
    symbol_kind::KEY,
    symbol_kind::NULL,
    symbol_kind::ENUM_MEMBER,
    symbol_kind::STRUCT,
    symbol_kind::EVENT,
    symbol_kind::OPERATOR,
    symbol_kind::TYPE_PARAMETER,
];

#[derive(Debug, Clone, Deserialize)]
pub struct HoverResult {
    pub contents: HoverContents,
    #[serde(default)]
    #[allow(dead_code)]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum HoverContents {
    Scalar(MarkedStringOrMarkup),
    Array(Vec<MarkedStringOrMarkup>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MarkedStringOrMarkup {
    Str(String),
    Markup {
        #[allow(dead_code)]
        kind: Option<String>,
        value: String,
    },
}

impl MarkedStringOrMarkup {
    pub fn text(&self) -> &str {
        match self {
            MarkedStringOrMarkup::Str(s) => s,
            MarkedStringOrMarkup::Markup { value, .. } => value,
        }
    }
}

impl HoverContents {
    pub fn to_text(&self) -> String {
        match self {
            HoverContents::Scalar(s) => s.text().to_string(),
            HoverContents::Array(v) => v
                .iter()
                .map(|s| s.text().to_string())
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<u32>,
    pub code: Option<serde_json::Value>,
    pub source: Option<String>,
    pub message: String,
}

/// Result of `textDocument/diagnostic` (LSP 3.17 pull diagnostics). Only the
/// "full" report (`items` present) carries diagnostics; an "unchanged"
/// report means the client's cached result (keyed by `resultId`) is still
/// valid. This tool never sends a `previousResultId`, so a spec-compliant
/// server has nothing to report unchanged against; `items` defaults to
/// empty for that case rather than erroring.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DocumentDiagnosticReport {
    #[serde(default)]
    pub items: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallHierarchyIncomingCall {
    pub from: CallHierarchyItem,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallHierarchyOutgoingCall {
    pub to: CallHierarchyItem,
}

/// `TypeHierarchyItem` has the exact same shape as `CallHierarchyItem` per
/// the LSP spec (name/kind/uri/range/selectionRange/detail) — reused rather
/// than duplicated.
pub type TypeHierarchyItem = CallHierarchyItem;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TextEdit {
    pub range: Range,
    #[serde(rename = "newText")]
    pub new_text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionedTextDocumentIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextDocumentEdit {
    #[serde(rename = "textDocument")]
    pub text_document: VersionedTextDocumentIdentifier,
    pub edits: Vec<TextEdit>,
}

/// A `documentChanges` entry is either a plain `TextDocumentEdit` or a file
/// operation (`CreateFile`/`RenameFile`/`DeleteFile`, distinguished by a
/// `kind` field TextDocumentEdit doesn't have) — this tool only applies
/// text edits; file operations are surfaced to the caller as skipped rather
/// than silently dropped, since a rename that renames a *file* as well as
/// its symbol would otherwise look complete when it isn't.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DocumentChangeOp {
    Edit(TextDocumentEdit),
    // The payload itself is never inspected — matching this variant at all
    // (rather than the operation's specific shape) is the signal that it's
    // an unsupported file operation to count and warn about.
    FileOp(#[allow(dead_code)] serde_json::Value),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkspaceEdit {
    #[serde(default)]
    pub changes: Option<std::collections::HashMap<String, Vec<TextEdit>>>,
    #[serde(default, rename = "documentChanges")]
    pub document_changes: Option<Vec<DocumentChangeOp>>,
}
