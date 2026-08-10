use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use lsp::text_pos::{byte_to_utf16_col, char_to_utf16_col};
use regex::Regex;

/// Patterns that don't depend on the symbol being searched for, compiled
/// once. `normalize_whitespace` in particular is called once per line of
/// the file being scanned, so recompiling `\s+` there meant one regex
/// compilation per line on every `--scope`/`--find` command.
fn ws_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").unwrap())
}

fn line_range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+),(\d+)$").unwrap())
}

fn single_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)$").unwrap())
}

fn numeric_scope_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+(,\d+)?$").unwrap())
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedPosition {
    pub line: u32,
    pub character: u32,
}

pub fn resolve_locate(
    content: &str,
    scope: Option<&str>,
    find: Option<&str>,
) -> Result<ResolvedPosition> {
    let lines: Vec<&str> = content.split('\n').collect();

    let mut find = find.map(|s| s.to_string());
    let is_numeric_scope = scope
        .map(|s| numeric_scope_re().is_match(s))
        .unwrap_or(false);

    if let Some(s) = scope {
        if !is_numeric_scope && find.is_none() {
            find = s.split('.').next_back().map(|s| s.to_string());
        }
    }

    let (start_line, end_line) = resolve_scope(scope, &lines)?;
    resolve_position(&lines, start_line, end_line, find.as_deref())
}

fn resolve_scope(scope: Option<&str>, lines: &[&str]) -> Result<(usize, usize)> {
    let Some(scope) = scope else {
        return Ok((0, lines.len().saturating_sub(1)));
    };

    if let Some(caps) = line_range_re().captures(scope) {
        let start: i64 = caps[1].parse::<i64>()? - 1;
        let raw_end: i64 = caps[2].parse()?;
        let end = if raw_end == 0 {
            lines.len() as i64 - 1
        } else {
            raw_end - 1
        };
        let start = start.max(0) as usize;
        let end = (end.min(lines.len() as i64 - 1)).max(0) as usize;
        return Ok((start, end));
    }

    if let Some(caps) = single_line_re().captures(scope) {
        let line: i64 = caps[1].parse::<i64>()? - 1;
        let clamped = line.max(0).min(lines.len() as i64 - 1).max(0) as usize;
        return Ok((clamped, clamped));
    }

    resolve_symbol_path(scope, lines)
}

fn resolve_symbol_path(symbol_path: &str, lines: &[&str]) -> Result<(usize, usize)> {
    let parts: Vec<&str> = symbol_path.split('.').collect();
    let first_name = parts[0];

    let first_line = find_symbol_definition(first_name, lines, 0, lines.len().saturating_sub(1))
        .ok_or_else(|| anyhow!("Symbol not found: {first_name}"))?;

    if parts.len() == 1 {
        return Ok((first_line, lines.len().saturating_sub(1)));
    }

    let nested_name = parts[1..].join(".");
    let nested_line = find_symbol_definition(
        &nested_name,
        lines,
        first_line + 1,
        lines.len().saturating_sub(1),
    )
    .ok_or_else(|| anyhow!("Nested symbol not found: {nested_name} within {first_name}"))?;

    Ok((nested_line, lines.len().saturating_sub(1)))
}

fn find_symbol_definition(name: &str, lines: &[&str], start: usize, end: usize) -> Option<usize> {
    let esc = regex::escape(name);
    // Anchored on a declaration keyword, so a match is unambiguously a
    // definition. Covers TS/JS, Python, Go, Rust, C-family, Kotlin, Ruby,
    // C#, Zig and Lua declaration forms.
    let declarations = [
        Regex::new(&format!(
            r"(?:class|function|const|let|var|interface|type|enum|struct|trait|impl|union|module|object|record|namespace|fn|def|func|val|local)\s+{esc}\b"
        ))
        .unwrap(),
        // `def self.name`, `pub fn name`, and similar, where a modifier sits
        // between the keyword and the name.
        Regex::new(&format!(r"(?:def\s+self\.|function\s+[\w.:]+[.:]){esc}\b")).unwrap(),
    ];
    // Unanchored: matches `name(` or `name = ...`. Necessary for method
    // shorthand (`greet() {}`) and assigned lambdas, but it also matches a
    // plain *call* — `main();` — so it is only consulted after no
    // declaration matched anywhere in range. Otherwise a call site above
    // the definition wins and every downstream command resolves to the
    // wrong line.
    let fallback = Regex::new(&format!(
        r"\b{esc}\s*(?:\(|=\s*(?:function|async function|\(|\())"
    ))
    .unwrap();

    let last = end.min(lines.len().saturating_sub(1));
    for i in start..=last {
        let line = lines.get(i).copied().unwrap_or("");
        if declarations.iter().any(|p| p.is_match(line)) {
            return Some(i);
        }
    }
    for i in start..=last {
        let line = lines.get(i).copied().unwrap_or("");
        if fallback.is_match(line) {
            return Some(i);
        }
    }
    None
}

fn resolve_position(
    lines: &[&str],
    start_line: usize,
    end_line: usize,
    find: Option<&str>,
) -> Result<ResolvedPosition> {
    let Some(find) = find else {
        let line_str = lines.get(start_line).copied().unwrap_or("");
        // `str::find` yields a *byte* offset; LSP wants UTF-16 code units.
        // This branch used to return the byte offset raw while the
        // `find`-pattern branch below returned a `char` offset — two
        // different units out of the same function, neither of them the
        // one the protocol specifies.
        let byte_col = line_str.find(|c: char| !c.is_whitespace()).unwrap_or(0);
        return Ok(ResolvedPosition {
            line: start_line as u32,
            character: byte_to_utf16_col(line_str, byte_col),
        });
    };

    let cursor_marker = "<|>";
    let cursor_idx = find.find(cursor_marker);
    let pattern_without_cursor = find.replace(cursor_marker, "");
    let normalized_pattern = normalize_whitespace(&pattern_without_cursor);

    for i in start_line..=end_line.min(lines.len().saturating_sub(1)) {
        let line = lines.get(i).copied().unwrap_or("");
        let normalized_line = normalize_whitespace(line);
        if normalized_line.contains(&normalized_pattern) {
            // `find_character_offset` works in `char` units because the
            // whitespace-normalization walk it does is naturally
            // character-oriented; convert at this single exit point.
            let char_col = find_character_offset(line, &pattern_without_cursor, cursor_idx);
            return Ok(ResolvedPosition {
                line: i as u32,
                character: char_to_utf16_col(line, char_col),
            });
        }
    }

    bail!(
        "Pattern not found in scope lines {}-{}: {:?}",
        start_line + 1,
        end_line + 1,
        find
    );
}

fn find_character_offset(original_line: &str, pattern: &str, cursor_idx: Option<usize>) -> usize {
    let orig: Vec<char> = original_line.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();

    let Some(cursor_idx) = cursor_idx else {
        return find_match_start_in_original(original_line, pattern);
    };

    let match_start = find_match_start_in_original(original_line, pattern);
    // cursor_idx is a byte index into `find` (before removing marker); pattern is ascii-heavy so
    // approximate by char count up to that byte offset in `pattern`.
    let pattern_before_cursor_chars = pattern[..cursor_idx.min(pattern.len())].chars().count();

    let mut orig_idx = match_start;
    let mut pat_idx = 0usize;

    while pat_idx < pattern_before_cursor_chars && orig_idx < orig.len() {
        let pc = pat[pat_idx.min(pat.len().saturating_sub(1))];
        let oc = orig[orig_idx];
        let pc_ws = pc.is_whitespace();
        let oc_ws = oc.is_whitespace();

        if pc_ws && oc_ws {
            while pat_idx < pattern_before_cursor_chars
                && pat.get(pat_idx).map(|c| c.is_whitespace()).unwrap_or(false)
            {
                pat_idx += 1;
            }
            while orig_idx < orig.len() && orig[orig_idx].is_whitespace() {
                orig_idx += 1;
            }
        } else if !pc_ws && !oc_ws {
            pat_idx += 1;
            orig_idx += 1;
        } else if oc_ws {
            while orig_idx < orig.len() && orig[orig_idx].is_whitespace() {
                orig_idx += 1;
            }
        } else {
            while pat_idx < pattern_before_cursor_chars
                && pat.get(pat_idx).map(|c| c.is_whitespace()).unwrap_or(false)
            {
                pat_idx += 1;
            }
        }
    }

    while orig_idx < orig.len() && orig[orig_idx].is_whitespace() {
        orig_idx += 1;
    }

    orig_idx
}

fn find_match_start_in_original(original_line: &str, pattern: &str) -> usize {
    let normalized_line = normalize_whitespace(original_line);
    let normalized_pattern = normalize_whitespace(pattern);
    let Some(norm_match_start) = normalized_line.find(&normalized_pattern) else {
        return 0;
    };
    // find() gives a byte offset into normalized_line; convert to char offset first
    let norm_char_offset = normalized_line[..norm_match_start].chars().count();
    map_normalized_offset(original_line, norm_char_offset)
}

fn map_normalized_offset(original: &str, normalized_offset: usize) -> usize {
    let mut normalized_count = 0usize;
    let mut in_whitespace = false;
    let chars: Vec<char> = original.chars().collect();

    for (i, &ch) in chars.iter().enumerate() {
        let is_ws = ch.is_whitespace();
        if is_ws {
            if !in_whitespace {
                if normalized_count == normalized_offset {
                    return i;
                }
                normalized_count += 1;
                in_whitespace = true;
            }
        } else {
            in_whitespace = false;
            if normalized_count == normalized_offset {
                return i;
            }
            normalized_count += 1;
        }
    }

    normalized_offset.min(chars.len())
}

fn normalize_whitespace(s: &str) -> String {
    ws_re().replace_all(s, " ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "class Foo {\n  bar() {\n    return 1;\n  }\n}\n";

    #[test]
    fn resolves_plain_line_number() {
        let pos = resolve_locate(SAMPLE, Some("2"), None).unwrap();
        assert_eq!(pos.line, 1);
    }

    #[test]
    fn resolves_line_range_scope_with_find() {
        let pos = resolve_locate(SAMPLE, Some("1,5"), Some("return 1")).unwrap();
        assert_eq!(pos.line, 2);
    }

    #[test]
    fn resolves_symbol_path() {
        let pos = resolve_locate(SAMPLE, Some("Foo.bar"), None).unwrap();
        assert_eq!(pos.line, 1);
    }

    /// Reads the character at a resolved position, interpreting
    /// `pos.character` as the UTF-16 offset it is. Tests used to index the
    /// line by `pos.character` as a byte offset directly, which only
    /// happened to work because every fixture was ASCII.
    fn char_at(content: &str, pos: ResolvedPosition) -> char {
        let line = content.split('\n').nth(pos.line as usize).unwrap();
        let byte = lsp::text_pos::utf16_col_to_byte(line, pos.character);
        line[byte..].chars().next().unwrap()
    }

    #[test]
    fn resolves_cursor_marker_position() {
        let pos = resolve_locate(SAMPLE, None, Some("return <|>1;")).unwrap();
        assert_eq!(pos.line, 2);
        assert_eq!(char_at(SAMPLE, pos), '1');
    }

    // --- position units -----------------------------------------------
    // LSP columns are UTF-16 code units. For ASCII and the whole BMP that
    // equals the char count, which is why the old char-offset
    // approximation went unnoticed; an astral character (2 UTF-16 units,
    // 1 char) is where the two diverge.

    #[test]
    fn find_position_is_in_utf16_units_not_chars() {
        // "𝕏" is one char but two UTF-16 code units, so `foo` sits at char
        // 19 and UTF-16 column 20.
        let content = "const 𝕏 = 1; const foo = 2;\n";
        let pos = resolve_locate(content, None, Some("foo")).unwrap();
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 20);
        assert_eq!(char_at(content, pos), 'f');
    }

    #[test]
    fn indentation_position_is_in_utf16_units_not_bytes() {
        // Leading tab, then an astral char. The first non-whitespace byte
        // offset is 1; as UTF-16 that is also 1 — but the old code returned
        // a raw byte offset, which diverges as soon as the indentation
        // itself contains multi-byte characters.
        let content = "  𝕏 = 1;\n";
        let pos = resolve_locate(content, Some("1"), None).unwrap();
        assert_eq!(pos.character, 2);
        assert_eq!(char_at(content, pos), '𝕏');
    }

    #[test]
    fn bmp_characters_do_not_shift_the_column() {
        let content = "const café = 1; const bar = 2;\n";
        let pos = resolve_locate(content, None, Some("bar")).unwrap();
        assert_eq!(pos.character, 22);
        assert_eq!(char_at(content, pos), 'b');
    }

    // --- definition vs. call site -------------------------------------

    #[test]
    fn a_call_site_above_the_definition_does_not_win() {
        // The unanchored `name(` pattern matches `main();` just as happily
        // as it matches the real definition. Scanning for declaration
        // keywords first is what keeps line 0 from being returned here.
        let content = "main();\nfunction main() {}\n";
        let pos = resolve_locate(content, Some("main"), None).unwrap();
        assert_eq!(pos.line, 1);
    }

    #[test]
    fn call_site_still_resolves_when_there_is_no_declaration_form() {
        // Method shorthand has no declaration keyword, so the fallback
        // pattern still has to work.
        let content = "class A {\n  greet() {\n    return 1;\n  }\n}\n";
        let pos = resolve_locate(content, Some("greet"), None).unwrap();
        assert_eq!(pos.line, 1);
    }

    #[test]
    fn resolves_rust_struct_declarations() {
        // `struct`/`trait`/`impl` matched none of the original four
        // patterns, so `--scope User` failed outright on Rust sources
        // despite Rust being listed as fully supported.
        let content = "use std::fmt;\n\npub struct User {\n    name: String,\n}\n";
        let pos = resolve_locate(content, Some("User"), None).unwrap();
        assert_eq!(pos.line, 2);
    }

    #[test]
    fn resolves_rust_trait_declarations() {
        let content = "mod a;\n\npub trait Greeter {\n    fn greet(&self);\n}\n";
        let pos = resolve_locate(content, Some("Greeter"), None).unwrap();
        assert_eq!(pos.line, 2);
    }

    #[test]
    fn missing_symbol_is_an_error() {
        assert!(resolve_locate(SAMPLE, Some("DoesNotExist"), None).is_err());
    }

    #[test]
    fn whitespace_insensitive_find() {
        let content = "function   foo(a,   b) {\n  return a + b;\n}\n";
        let pos = resolve_locate(content, None, Some("function foo(a, b)")).unwrap();
        assert_eq!(pos.line, 0);
    }
}
