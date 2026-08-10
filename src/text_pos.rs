//! LSP position arithmetic: the single place this codebase converts between
//! byte offsets, `char` (Unicode scalar) offsets, and the UTF-16 code units
//! the Language Server Protocol actually specifies.
//!
//! This exists because the same conversion was previously written five
//! times with three different answers: `locate.rs` approximated LSP
//! `character` as a `char` count (and in one branch returned a raw *byte*
//! offset), `commands.rs::apply_text_edits` indexed a `Vec<char>` with a
//! server-supplied UTF-16 offset while writing files to disk, and each of
//! the four bundled servers under `src/servers/` carried its own correct
//! but duplicated copy.
//!
//! The units matter. LSP 3.17 says a position's `character` is an offset in
//! UTF-16 code units unless the client negotiates otherwise via
//! `general.positionEncodings` — which `lsp_client.rs::initialize` does not
//! do, so UTF-16 is what every server on both ends of this tool assumes.
//! For ASCII and the whole Basic Multilingual Plane (accents, CJK, Cyrillic)
//! a `char` count and a UTF-16 count are equal, which is why the old
//! approximation survived; they diverge only on astral-plane characters
//! (emoji, `𝕏`, CJK Extension B), where one `char` is two UTF-16 units.
//! That divergence silently corrupted files on `rename --apply`.

/// Byte offset within `line` → UTF-16 code-unit offset.
///
/// `byte_col` is clamped to the line length, and rounded *down* to a char
/// boundary rather than panicking, so a server that reports a position
/// inside a multi-byte character degrades to the character's start instead
/// of taking the process down.
pub fn byte_to_utf16_col(line: &str, byte_col: usize) -> u32 {
    let byte_col = clamp_to_char_boundary(line, byte_col);
    line[..byte_col].chars().map(|c| c.len_utf16() as u32).sum()
}

/// `char` (Unicode scalar) offset within `line` → UTF-16 code-unit offset.
pub fn char_to_utf16_col(line: &str, char_col: usize) -> u32 {
    line.chars()
        .take(char_col)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// UTF-16 code-unit offset within `line` → byte offset.
///
/// An offset past the end of the line clamps to the line length; an offset
/// that lands in the middle of a surrogate pair resolves to the start of
/// the character containing it.
pub fn utf16_col_to_byte(line: &str, utf16_col: u32) -> usize {
    let mut seen = 0u32;
    for (byte_idx, c) in line.char_indices() {
        if seen >= utf16_col {
            return byte_idx;
        }
        seen += c.len_utf16() as u32;
        // Overshooting means `utf16_col` pointed at the low half of this
        // character's surrogate pair. Resolve to the character's start
        // rather than past it: the offset still refers to this character.
        if seen > utf16_col {
            return byte_idx;
        }
    }
    line.len()
}

/// UTF-16 code-unit offset within `line` → `char` offset.
pub fn utf16_col_to_char(line: &str, utf16_col: u32) -> usize {
    let mut seen = 0u32;
    for (char_idx, c) in line.chars().enumerate() {
        if seen >= utf16_col {
            return char_idx;
        }
        seen += c.len_utf16() as u32;
        // See `utf16_col_to_byte`: a mid-surrogate offset belongs to this
        // character, not the next one.
        if seen > utf16_col {
            return char_idx;
        }
    }
    line.chars().count()
}

/// Total width of `line` in UTF-16 code units.
pub fn utf16_len(line: &str) -> u32 {
    line.chars().map(|c| c.len_utf16() as u32).sum()
}

fn clamp_to_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Precomputed line-start byte offsets for a document.
///
/// Built once per document, this turns "give me row N" from a scan of the
/// whole text into an index lookup. The bundled servers previously called
/// `text.lines().nth(row)` once per `Point` conversion, twice per range,
/// and twice per emitted symbol — four full rescans per symbol, i.e.
/// `O(symbols × file_size)` for a single `documentSymbol` request.
///
/// Line content excludes the terminator, and a trailing `\r` is trimmed so
/// a CRLF document reports the same column a server would.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { starts }
    }

    /// Number of lines. A trailing newline yields a final empty line, which
    /// matches how both `str::split('\n')` and tree-sitter count rows.
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// The text of row `row`, without its line terminator, or `""` if the
    /// row is out of range.
    pub fn line<'a>(&self, text: &'a str, row: usize) -> &'a str {
        let Some(&start) = self.starts.get(row) else {
            return "";
        };
        let end = self
            .starts
            .get(row + 1)
            .map(|&next| next - 1) // drop the '\n' that began the next line
            .unwrap_or(text.len());
        text[start..end]
            .strip_suffix('\r')
            .unwrap_or(&text[start..end])
    }

    /// Byte offset where row `row` begins; clamps to the end of the text.
    pub fn line_start(&self, text: &str, row: usize) -> usize {
        self.starts.get(row).copied().unwrap_or(text.len())
    }

    /// (row, UTF-16 column) → absolute byte offset.
    pub fn position_to_byte(&self, text: &str, row: usize, utf16_col: u32) -> usize {
        let start = self.line_start(text, row);
        start + utf16_col_to_byte(self.line(text, row), utf16_col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // "𝕏" is U+1D54F: 4 bytes, 2 UTF-16 code units, 1 char. It is the
    // whole reason this module exists, so it appears in most cases below.
    const ASTRAL: &str = "𝕏";

    #[test]
    fn ascii_offsets_agree_across_all_three_units() {
        let line = "let foo = 1;";
        assert_eq!(byte_to_utf16_col(line, 4), 4);
        assert_eq!(char_to_utf16_col(line, 4), 4);
        assert_eq!(utf16_col_to_byte(line, 4), 4);
        assert_eq!(utf16_col_to_char(line, 4), 4);
        assert_eq!(utf16_len(line), 12);
    }

    #[test]
    fn bmp_chars_are_one_utf16_unit_each() {
        // é and 中 are why the old char-count approximation survived so
        // long: for everything in the BMP it is exactly right.
        let line = "é中x";
        assert_eq!(utf16_len(line), 3);
        assert_eq!(char_to_utf16_col(line, 2), 2);
        assert_eq!(utf16_col_to_char(line, 2), 2);
    }

    #[test]
    fn astral_char_is_two_utf16_units_but_one_char() {
        let line = format!("{ASTRAL}ab");
        assert_eq!(line.chars().count(), 3);
        assert_eq!(utf16_len(&line), 4);
        // The 'a' sits at char 1, byte 4, UTF-16 column 2.
        assert_eq!(char_to_utf16_col(&line, 1), 2);
        assert_eq!(byte_to_utf16_col(&line, 4), 2);
        assert_eq!(utf16_col_to_byte(&line, 2), 4);
        assert_eq!(utf16_col_to_char(&line, 2), 1);
    }

    #[test]
    fn offsets_past_the_end_clamp_instead_of_panicking() {
        let line = "abc";
        assert_eq!(utf16_col_to_byte(line, 99), 3);
        assert_eq!(utf16_col_to_char(line, 99), 3);
        assert_eq!(byte_to_utf16_col(line, 99), 3);
    }

    #[test]
    fn byte_offset_inside_a_multibyte_char_rounds_down_to_its_start() {
        let line = format!("{ASTRAL}a");
        // Byte 2 is inside the 4-byte 𝕏; treat it as the start of 𝕏 (col 0)
        // rather than slicing at a non-char-boundary and panicking.
        assert_eq!(byte_to_utf16_col(&line, 2), 0);
    }

    #[test]
    fn utf16_offset_inside_a_surrogate_pair_resolves_to_the_char_start() {
        let line = format!("{ASTRAL}a");
        // Column 1 is the low half of 𝕏's surrogate pair.
        assert_eq!(utf16_col_to_byte(&line, 1), 0);
        assert_eq!(utf16_col_to_char(&line, 1), 0);
    }

    #[test]
    fn line_index_returns_rows_without_terminators() {
        let text = "alpha\nbeta\ngamma";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 3);
        assert_eq!(idx.line(text, 0), "alpha");
        assert_eq!(idx.line(text, 1), "beta");
        assert_eq!(idx.line(text, 2), "gamma");
        assert_eq!(idx.line(text, 99), "");
    }

    #[test]
    fn line_index_strips_carriage_returns_so_crlf_columns_match() {
        let text = "alpha\r\nbeta\r\n";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line(text, 0), "alpha");
        assert_eq!(idx.line(text, 1), "beta");
        // Trailing newline produces a final empty row, matching split('\n').
        assert_eq!(idx.line_count(), 3);
        assert_eq!(idx.line(text, 2), "");
    }

    #[test]
    fn line_index_agrees_with_the_naive_scan_it_replaces() {
        let text = "one\ntwo\nthree\nfour";
        let idx = LineIndex::new(text);
        for row in 0..idx.line_count() {
            assert_eq!(idx.line(text, row), text.lines().nth(row).unwrap_or(""));
        }
    }

    #[test]
    fn position_to_byte_accounts_for_astral_chars_on_earlier_columns() {
        let text = format!("{ASTRAL}ab\nsecond");
        let idx = LineIndex::new(&text);
        // Row 0, UTF-16 column 2 is 'a' — byte 4, not byte 2.
        assert_eq!(idx.position_to_byte(&text, 0, 2), 4);
        // Row 1 starts after "𝕏ab\n" = 4 + 2 + 1 = 7 bytes.
        assert_eq!(idx.position_to_byte(&text, 1, 0), 7);
    }

    #[test]
    fn empty_text_has_one_empty_row() {
        let text = "";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 1);
        assert_eq!(idx.line(text, 0), "");
        assert_eq!(idx.position_to_byte(text, 0, 0), 0);
    }
}
