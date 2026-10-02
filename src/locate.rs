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
    if scope.is_none() && find.is_none() {
        // Without either there is no position to ask about. This used to
        // fall through to "the first non-blank character of line 1", so
        // `lsp definition file.ts` answered a question nobody asked and
        // exited 0 with an empty result.
        bail!("--scope (a line, a line range, or a symbol name) or --find is required to pick a position");
    }
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

/// The number of real lines: `split('\n')` also yields an empty entry
/// after a trailing newline, which is not a line anyone can point at.
fn line_count(lines: &[&str]) -> usize {
    match lines.last() {
        Some(last) if last.is_empty() && lines.len() > 1 => lines.len() - 1,
        _ => lines.len(),
    }
}

/// Resolves `scope` to an inclusive, 0-based line range.
pub fn resolve_scope(scope: Option<&str>, lines: &[&str]) -> Result<(usize, usize)> {
    let count = line_count(lines);
    let Some(scope) = scope else {
        return Ok((0, count.saturating_sub(1)));
    };
    let out_of_range =
        |line: u64| anyhow!("line {line} is out of range: the file has {count} line(s)");

    if let Some(caps) = line_range_re().captures(scope) {
        let start: u64 = caps[1].parse()?;
        let raw_end: u64 = caps[2].parse()?;
        if start == 0 || start as usize > count {
            return Err(out_of_range(start));
        }
        // `N,0` means "to the end of the file"; an end past the last line
        // is clamped, since "from here to somewhere past the end" is still
        // a meaningful request.
        let end = if raw_end == 0 {
            count
        } else {
            (raw_end as usize).min(count)
        };
        if raw_end != 0 && (raw_end as usize) < start as usize {
            bail!("line range {scope} ends before it starts");
        }
        return Ok((start as usize - 1, end - 1));
    }

    if let Some(caps) = single_line_re().captures(scope) {
        let line: u64 = caps[1].parse()?;
        if line == 0 || line as usize > count {
            return Err(out_of_range(line));
        }
        return Ok((line as usize - 1, line as usize - 1));
    }

    resolve_symbol_path(scope, lines)
}

/// Resolves `A.B.C` one level at a time, each within the previous one's
/// block, and returns the last one's block.
///
/// Each nested lookup used to search from the parent's line to the end of
/// the *file*, so `UserOptions.greet` resolved to `User.greet` in the next
/// class down; and a plain `--scope Name` let `--find` match anywhere below
/// it. Both are now bounded by the symbol's block (`block_end`).
///
/// A type's members don't always live inside its declaration: Rust puts
/// methods in `impl Type` / `impl Trait for Type` blocks, and Go declares
/// them at top level with a receiver (`func (u *User) Greet()`). Those are
/// searched too, so `User.greet` works in both.
fn resolve_symbol_path(symbol_path: &str, lines: &[&str]) -> Result<(usize, usize)> {
    let parts: Vec<&str> = symbol_path.split('.').collect();
    let last = line_count(lines).saturating_sub(1);
    let mut ranges = vec![(0, last)];
    let mut receiver: Option<&str> = None;
    let mut found = (0, last);
    for (depth, name) in parts.iter().enumerate() {
        let lookup = find_symbol_definition(name, lines, &ranges, receiver);
        // A parent that isn't declared in this file can still own members
        // that are: a Go type's methods are often in a different file from
        // the type. Carry on with it as the receiver; if no member matches
        // either, the parent's own "not found" is the error reported.
        if let (Err(Lookup::NotFound), Some(next)) = (&lookup, parts.get(depth + 1)) {
            if find_symbol_definition(next, lines, &[], Some(name)).is_ok() {
                ranges = vec![];
                receiver = Some(name);
                continue;
            }
        }
        let line = lookup.map_err(|e| match e {
            Lookup::NotFound if depth == 0 => anyhow!("Symbol not found: {name}"),
            Lookup::NotFound => anyhow!(
                "Nested symbol not found: {name} within {}",
                parts[..depth].join(".")
            ),
            Lookup::Ambiguous(candidates) => {
                let listed: Vec<String> = candidates
                    .iter()
                    .map(|&l| format!("  line {}: {}", l + 1, lines[l].trim()))
                    .collect();
                anyhow!(
                    "`{name}` is declared in {} different places in scope:\n{}\nDisambiguate with its parent (`Parent.{name}`) or a line number (`--scope {}`).",
                    candidates.len(),
                    listed.join("\n"),
                    candidates[0] + 1
                )
            }
        })?;
        found = (line, block_end(lines, line));
        // The next part is searched for inside this one's body, and in any
        // blocks elsewhere that belong to it.
        ranges = vec![];
        if found.1 > line {
            ranges.push((line + 1, found.1));
        }
        ranges.extend(
            associated_blocks(name, lines)
                .into_iter()
                .filter(|&(start, _)| start != line)
                .filter_map(|(start, end)| (end > start).then_some((start + 1, end))),
        );
        receiver = Some(name);
    }
    Ok(found)
}

/// `impl Name`, `impl<T> Name<T>`, `impl Trait for Name` blocks (Rust):
/// where a type's methods are declared.
fn associated_blocks(name: &str, lines: &[&str]) -> Vec<(usize, usize)> {
    let re = Regex::new(&format!(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?impl\b(?:<[^>]*>)?\s+(?:[\w:<>, ]+\s+for\s+)?(?:[\w]+::)*{}\b",
        regex::escape(name)
    ))
    .unwrap();
    (0..line_count(lines))
        .filter(|&i| re.is_match(code_part(lines[i])))
        .map(|i| (i, block_end(lines, i)))
        .collect()
}

/// The part of a line that is code: everything before a line comment, and
/// nothing at all for a line that is entirely a comment. A declaration
/// keyword in a comment (`}  // namespace foo`, `# class Foo is gone`) is
/// not a declaration.
fn code_part(line: &str) -> &str {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("--")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        return "";
    }
    // `//` starts a comment only at the start of a token, so a URL inside
    // a string (`"http://x"`) doesn't cut the line short.
    let bytes = line.as_bytes();
    let mut in_string: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match in_string {
            Some(q) if b == q && (i == 0 || bytes[i - 1] != b'\\') => in_string = None,
            Some(_) => {}
            None if b == b'"' || b == b'`' => in_string = Some(b),
            None if b == b'/' && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            None => {}
        }
    }
    line
}

/// The last line of the block that starts at `decl`.
///
/// Brace-delimited code is measured by matching braces, from the first `{`
/// that opens on the declaration or its signature (a multi-line parameter
/// list, or an Allman-style brace on the next line). Anything else —
/// Python, Ruby, Lua, a one-line `= expr` — by indentation. Both are
/// heuristics, like the declaration patterns themselves, but each holds
/// where the other fails: indentation alone ended a function at the `) ->
/// Self {` line of its own multi-line signature, and cannot see the extent
/// of a C++ namespace whose contents are not indented.
fn block_end(lines: &[&str], decl: usize) -> usize {
    brace_block_end(lines, decl).unwrap_or_else(|| indent_block_end(lines, decl))
}

fn brace_block_end(lines: &[&str], decl: usize) -> Option<usize> {
    let count = line_count(lines);
    let mut braces: i64 = 0;
    let mut parens: i64 = 0;
    let mut opened = false;
    for (i, line) in lines.iter().enumerate().take(count).skip(decl) {
        let code = code_part(line);
        let trimmed = code.trim();
        if !opened && parens == 0 && i > decl {
            // The signature is complete and no brace opened on it: only an
            // Allman brace on its own line may still start the body.
            if !trimmed.starts_with('{') {
                return None;
            }
        }
        let mut in_string: Option<char> = None;
        let mut prev = ' ';
        for c in code.chars() {
            match in_string {
                Some(q) if c == q && prev != '\\' => in_string = None,
                Some(_) => {}
                None => match c {
                    '"' | '`' => in_string = Some(c),
                    '(' | '[' => parens += 1,
                    ')' | ']' => parens -= 1,
                    '{' => {
                        braces += 1;
                        opened = true;
                    }
                    '}' => braces -= 1,
                    _ => {}
                },
            }
            prev = c;
        }
        if opened && braces <= 0 {
            // `def f(a={}):` — braces in a Python signature, not a block.
            if trimmed.ends_with(':') {
                return None;
            }
            return Some(i);
        }
        if !opened && parens <= 0 && trimmed.ends_with(';') {
            // A bodiless declaration: `function f(a: string): void;`.
            return Some(i);
        }
        if !opened && trimmed.ends_with(':') && parens <= 0 {
            return None;
        }
    }
    None
}

fn indent_block_end(lines: &[&str], decl: usize) -> usize {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let decl_indent = indent(lines[decl]);
    let count = line_count(lines);
    let mut last_inside = decl;
    for (i, line) in lines.iter().enumerate().take(count).skip(decl + 1) {
        let trimmed = code_part(line).trim();
        if line.trim().is_empty() {
            continue;
        }
        if trimmed.is_empty() || indent(line) > decl_indent {
            // Deeper, or a comment line: part of the block.
            last_inside = i;
            continue;
        }
        if indent(line) < decl_indent {
            return last_inside;
        }
        // Same indentation as the declaration.
        if is_closer(trimmed) {
            return i;
        }
        // `) -> int:` closing a multi-line signature continues it.
        if trimmed.starts_with(')') || trimmed.starts_with(']') {
            last_inside = i;
            continue;
        }
        return last_inside;
    }
    last_inside
}

/// A line that only closes a block: `}`, `});`, `]`, `end`, `end # done`.
fn is_closer(trimmed: &str) -> bool {
    let word = trimmed.split_whitespace().next().unwrap_or("");
    word == "end"
        || word.starts_with("end;")
        || (!trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| matches!(c, '}' | ')' | ']' | ';' | ',')))
}

enum Lookup {
    NotFound,
    /// Declarations in different enclosing blocks, at these (0-based) lines.
    Ambiguous(Vec<usize>),
}

fn find_symbol_definition(
    name: &str,
    lines: &[&str],
    ranges: &[(usize, usize)],
    receiver: Option<&str>,
) -> std::result::Result<usize, Lookup> {
    let esc = regex::escape(name);
    // What may follow the name: not `.`/`::`/`:ident`, so `function M.add`
    // and `function M:sub` (Lua) don't count as declarations of `M`, while
    // Python's `class A:` still does.
    let tail = r"(?:$|[^\w.:]|:(?:$|[^\w:]))";
    // Anchored on a declaration keyword, so a match is unambiguously a
    // definition. Covers TS/JS, Python, Go, Rust, C-family, Kotlin, Ruby,
    // C#, Zig and Lua declaration forms.
    let declarations = [
        Regex::new(&format!(
            r"(?:^|[^\w])(?:class|function|const|let|var|interface|type|enum|struct|trait|union|mod|module|object|record|namespace|fn|fun|def|func|val|local)\s+{esc}{tail}"
        ))
        .unwrap(),
        // `def self.name` (Ruby), `function Mod.name` / `Mod:name` (Lua).
        Regex::new(&format!(r"(?:def\s+self\.|function\s+[\w.:]+[.:]){esc}{tail}")).unwrap(),
    ];
    // `impl Name` (Rust) declares nothing new: it is a block *for* a type
    // declared elsewhere. It counts only when no real declaration exists.
    let impl_block = Regex::new(&format!(r"\bimpl(?:<[^>]*>)?\s+{esc}\b")).unwrap();
    // Members declared outside the parent's body, matched only as members
    // of the type named by the previous path segment, anywhere in the
    // file: a Go method (`func (u *User) Greet(`), a Lua module function
    // (`function M.add(` / `function M:sub(`), a JS prototype method.
    let qualified = receiver.map(|r| {
        let r = regex::escape(r);
        Regex::new(&format!(
            r"^\s*(?:func\s*\(\s*\w*\s*\*?\s*{r}(?:\[[^\]]*\])?\s*\)\s*{esc}\b|(?:local\s+)?function\s+{r}[.:]{esc}\b|{r}\.prototype\.{esc}\s*=)"
        ))
        .unwrap()
    });
    // A member declared without a keyword — TS/JS method shorthand
    // (`area() {`), Java/C#/C++ methods (`public double area() {`) —
    // recognized by starting its line, after modifiers and a return type,
    // with the name and `(`, and by not being a statement (`;`). A plain
    // call statement has a receiver (`this.area()`) or a keyword before it
    // (`return area()`), which this excludes.
    let member = Regex::new(&format!(
        r"^\s*(?:@\w+\s+)*(?:[\w<>\[\],?*&:]+\s+)*?{esc}\s*(?:<[^>]*>)?\("
    ))
    .unwrap();
    let statement_keyword = Regex::new(
        r"^\s*(?:return|await|yield|throw|new|else|if|case|echo|print|puts|assert|raise)\b",
    )
    .unwrap();
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

    let last = line_count(lines).saturating_sub(1);
    let in_ranges = |re: &Regex| -> Vec<usize> {
        let mut hits: Vec<usize> = ranges
            .iter()
            .flat_map(|&(start, end)| start..=end.min(last))
            .filter(|&i| re.is_match(code_part(lines[i])))
            .collect();
        hits.sort_unstable();
        hits.dedup();
        hits
    };
    let mut declared: Vec<usize> = declarations.iter().flat_map(&in_ranges).collect();
    if let Some(re) = &qualified {
        declared.extend((0..=last).filter(|&i| re.is_match(code_part(lines[i]))));
    }
    declared.sort_unstable();
    declared.dedup();
    if declared.is_empty() {
        declared = in_ranges(&impl_block);
    }
    if declared.is_empty() {
        declared = in_ranges(&member)
            .into_iter()
            .filter(|&i| {
                let code = code_part(lines[i]).trim_end();
                !code.ends_with(';') && !statement_keyword.is_match(code)
            })
            .collect();
    }
    if let Some(&first) = declared.first() {
        // Several declarations directly inside the *same* block are one
        // symbol's variants — overload signatures, a property's getter and
        // setter, a `const` and the `type` of the same name — and the first
        // stands for them all. Declarations in *different* blocks (an
        // `area` method in two classes) are different symbols: an agent
        // acting on a silent pick of one is worse off than one told to
        // choose, so that is an error listing the candidates.
        let parent = |l: usize| enclosing_line(lines, l);
        if declared.iter().all(|&l| parent(l) == parent(first)) {
            return Ok(first);
        }
        return Err(Lookup::Ambiguous(declared));
    }
    // Call sites and shorthand: the first one is the best guess there is.
    in_ranges(&fallback)
        .first()
        .copied()
        .ok_or(Lookup::NotFound)
}

/// The nearest line above `line` that is indented less than it — the
/// block it sits in — or `None` at top level.
fn enclosing_line(lines: &[&str], line: usize) -> Option<usize> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let own = indent(lines[line]);
    (0..line)
        .rev()
        .find(|&i| !code_part(lines[i]).trim().is_empty() && indent(lines[i]) < own)
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

    // --- scope bounds -------------------------------------------------

    #[test]
    fn no_scope_and_no_find_is_an_error() {
        let err = resolve_locate(SAMPLE, None, None).unwrap_err();
        assert!(err.to_string().contains("--scope"), "{err}");
    }

    #[test]
    fn a_line_past_the_end_is_an_error_not_the_last_line() {
        // SAMPLE has 5 lines (the empty string after its trailing newline
        // is not a sixth).
        assert!(resolve_locate(SAMPLE, Some("5"), None).is_ok());
        let err = resolve_locate(SAMPLE, Some("6"), None).unwrap_err();
        assert!(err.to_string().contains("5 line"), "{err}");
        assert!(resolve_locate(SAMPLE, Some("0"), None).is_err());
    }

    #[test]
    fn a_range_starting_past_the_end_is_an_error() {
        assert!(resolve_scope(Some("500,600"), &SAMPLE.split('\n').collect::<Vec<_>>()).is_err());
    }

    #[test]
    fn a_range_end_past_the_end_is_clamped_and_zero_means_end_of_file() {
        let lines: Vec<&str> = SAMPLE.split('\n').collect();
        assert_eq!(resolve_scope(Some("2,99"), &lines).unwrap(), (1, 4));
        assert_eq!(resolve_scope(Some("2,0"), &lines).unwrap(), (1, 4));
        assert!(resolve_scope(Some("4,2"), &lines).is_err());
    }

    #[test]
    fn find_respects_a_line_range_scope() {
        // `return 1` is on line 3; a scope of lines 1-2 must not reach it.
        assert!(resolve_locate(SAMPLE, Some("1,2"), Some("return 1")).is_err());
        assert_eq!(
            resolve_locate(SAMPLE, Some("3,3"), Some("return <|>1"))
                .unwrap()
                .character,
            11
        );
    }

    // --- symbol blocks ------------------------------------------------

    const TWO_CLASSES: &str = "\
interface UserOptions {
  name: string;
}

class User {
  greet() {
    return 1;
  }
}
";

    #[test]
    fn a_nested_path_does_not_escape_its_parent() {
        // `greet` exists only in `User`; looking for it inside
        // `UserOptions` used to search to the end of the file and find
        // `User.greet` instead.
        let err = resolve_locate(TWO_CLASSES, Some("UserOptions.greet"), None).unwrap_err();
        assert!(err.to_string().contains("Nested symbol not found"), "{err}");
        let pos = resolve_locate(TWO_CLASSES, Some("User.greet"), None).unwrap();
        assert_eq!(pos.line, 5);
    }

    #[test]
    fn find_inside_a_symbol_scope_stays_inside_the_symbol() {
        assert!(resolve_locate(TWO_CLASSES, Some("UserOptions"), Some("return 1")).is_err());
        let pos = resolve_locate(TWO_CLASSES, Some("User"), Some("return <|>1")).unwrap();
        assert_eq!((pos.line, pos.character), (6, 11));
    }

    #[test]
    fn block_end_handles_braces_indentation_and_allman_style() {
        let lines: Vec<&str> = TWO_CLASSES.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 2);
        assert_eq!(block_end(&lines, 4), 8);
        let py = "class A:\n    def f(self):\n        return 1\n\n    def g(self):\n        pass\nx = 1\n";
        let lines: Vec<&str> = py.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 5);
        assert_eq!(block_end(&lines, 1), 2);
        let allman = "class A\n{\n    void F() {}\n}\nclass B {}\n";
        let lines: Vec<&str> = allman.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 3);
    }

    #[test]
    fn three_level_paths_resolve_level_by_level() {
        let content = "mod outer {\n    struct Skip {}\n    mod inner {\n        fn target() {}\n    }\n}\nfn target() {}\n";
        let pos = resolve_locate(content, Some("outer.inner.target"), None).unwrap();
        assert_eq!(pos.line, 3);
    }

    // --- ambiguity ----------------------------------------------------

    #[test]
    fn a_name_declared_in_different_blocks_is_an_error_listing_the_candidates() {
        let content = "class A:\n    def area(self):\n        pass\n\nclass B:\n    def area(self):\n        pass\n";
        let err = resolve_locate(content, Some("area"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 different places"), "{err}");
        assert!(err.contains("line 2: def area(self):"), "{err}");
        assert!(err.contains("line 6:"), "{err}");
        // Qualified, it is unambiguous.
        assert_eq!(
            resolve_locate(content, Some("B.area"), None).unwrap().line,
            5
        );
    }

    #[test]
    fn methods_without_a_keyword_in_two_classes_are_ambiguous_too() {
        // TS method shorthand. `this.area()` in Circle's constructor is a
        // call, and must not be taken for a declaration.
        let content = "class Circle {\n  constructor() { this.area(); }\n  area() { return 1; }\n}\nclass Square {\n  area() { return 2; }\n}\n";
        let err = resolve_locate(content, Some("area"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 3:") && err.contains("line 6:"), "{err}");
        assert_eq!(
            resolve_locate(content, Some("Circle.area"), None)
                .unwrap()
                .line,
            2
        );
        assert_eq!(
            resolve_locate(content, Some("Square.area"), None)
                .unwrap()
                .line,
            5
        );
    }

    // --- variants of one symbol are not ambiguous ----------------------

    #[test]
    fn overloads_getters_setters_and_same_named_types_resolve_to_the_first() {
        let ts = "export function over(a: string): void;\nexport function over(a: number): void;\nexport function over(a: any) {}\n";
        assert_eq!(resolve_locate(ts, Some("over"), None).unwrap().line, 0);
        let py = "class Model:\n    @property\n    def name(self):\n        return 1\n\n    @name.setter\n    def name(self, v):\n        pass\n";
        assert_eq!(
            resolve_locate(py, Some("Model.name"), None).unwrap().line,
            2
        );
        let zod = "export const User = z.object({});\nexport type User = z.infer<typeof User>;\n";
        assert_eq!(resolve_locate(zod, Some("User"), None).unwrap().line, 0);
    }

    #[test]
    fn a_lua_module_table_is_not_confused_with_its_functions() {
        let lua = "local M = {}\n\nfunction M.add(a, b)\n  return a + b\nend\n\nfunction M:sub(b)\n  return 1\nend\n\nreturn M\n";
        assert_eq!(resolve_locate(lua, Some("M"), None).unwrap().line, 0);
        assert_eq!(resolve_locate(lua, Some("M.add"), None).unwrap().line, 2);
        assert_eq!(resolve_locate(lua, Some("M.sub"), None).unwrap().line, 6);
    }

    #[test]
    fn declarations_inside_comments_do_not_count() {
        let cpp = "namespace foo {\n\nclass Bar {\n  int x;\n};\n\n}  // namespace foo\n";
        assert_eq!(resolve_locate(cpp, Some("foo"), None).unwrap().line, 0);
        // An unindented namespace body still belongs to the namespace.
        assert_eq!(resolve_locate(cpp, Some("foo.Bar"), None).unwrap().line, 2);
    }

    // --- members declared outside their type's body --------------------

    #[test]
    fn rust_methods_are_found_in_impl_blocks() {
        let rs = "pub struct User {\n    name: String,\n}\n\nimpl User {\n    pub fn greet(&self) -> String {\n        self.name.clone()\n    }\n}\n\nimpl std::fmt::Display for User {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {\n        Ok(())\n    }\n}\n";
        assert_eq!(
            resolve_locate(rs, Some("User.greet"), None).unwrap().line,
            5
        );
        assert_eq!(resolve_locate(rs, Some("User.fmt"), None).unwrap().line, 11);
        assert_eq!(resolve_locate(rs, Some("User"), None).unwrap().line, 0);
    }

    #[test]
    fn go_methods_are_found_by_their_receiver() {
        let go = "type User struct {\n\tName string\n}\n\nfunc (u User) Greet() string {\n\treturn u.Name\n}\n\nfunc (o *Other) Greet() string {\n\treturn \"\"\n}\n";
        assert_eq!(
            resolve_locate(go, Some("User.Greet"), None).unwrap().line,
            4
        );
        assert_eq!(
            resolve_locate(go, Some("Other.Greet"), None).unwrap().line,
            8
        );
        // And `--find` inside the method's own scope works.
        let pos = resolve_locate(go, Some("User.Greet"), Some("return <|>u.Name")).unwrap();
        assert_eq!(pos.line, 5);
    }

    // --- multi-line signatures -----------------------------------------

    #[test]
    fn a_multi_line_signature_does_not_end_the_block() {
        let py = "def cached(\n    a: int,\n) -> int:\n    return a + 1\n\nx = 1\n";
        let lines: Vec<&str> = py.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 3);
        let ts =
            "export const handler = async (\n  req,\n) => {\n  return req;\n};\nconst other = 1;\n";
        let lines: Vec<&str> = ts.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 4);
        let rs = "pub fn new(\n    a: u32,\n) -> Self {\n    Self { a }\n}\nfn next() {}\n";
        let lines: Vec<&str> = rs.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 4);
        let kt = "data class P(\n    val x: Int,\n) {\n    fun go() = x\n}\n";
        assert_eq!(resolve_locate(kt, Some("P.go"), None).unwrap().line, 3);
        let cs = "class A\n{\n    void Run(\n        int a)\n    {\n        Go();\n    }\n}\n";
        let lines: Vec<&str> = cs.split('\n').collect();
        assert_eq!(block_end(&lines, 2), 6);
        assert_eq!(block_end(&lines, 0), 7);
    }

    #[test]
    fn a_one_line_member_does_not_swallow_its_parent_s_closing_brace() {
        let kt = "class User {\n    fun greet(): String = \"hi\"\n}\n";
        let lines: Vec<&str> = kt.split('\n').collect();
        assert_eq!(block_end(&lines, 1), 1);
        let ts = "interface I {\n  foo(): void;\n}\n";
        let lines: Vec<&str> = ts.split('\n').collect();
        assert_eq!(block_end(&lines, 1), 1);
    }

    #[test]
    fn ruby_and_lua_blocks_end_at_their_end_keyword() {
        let rb = "class A\n  def f\n    1\n  end # done\nend\nputs 1\n";
        let lines: Vec<&str> = rb.split('\n').collect();
        assert_eq!(block_end(&lines, 0), 4);
        assert_eq!(block_end(&lines, 1), 3);
    }

    #[test]
    fn a_rust_struct_and_its_impl_block_are_not_ambiguous() {
        let content =
            "pub struct User {\n    name: String,\n}\n\nimpl User {\n    fn greet(&self) {}\n}\n";
        assert_eq!(resolve_locate(content, Some("User"), None).unwrap().line, 0);
        // And methods are found inside the impl block when that's the only
        // declaration of the parent's name in scope.
        let only_impl = "impl User {\n    fn greet(&self) {}\n}\n";
        assert_eq!(
            resolve_locate(only_impl, Some("User.greet"), None)
                .unwrap()
                .line,
            1
        );
    }

    #[test]
    fn whitespace_insensitive_find() {
        let content = "function   foo(a,   b) {\n  return a + b;\n}\n";
        let pos = resolve_locate(content, None, Some("function foo(a, b)")).unwrap();
        assert_eq!(pos.line, 0);
    }
}
