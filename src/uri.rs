//! `file:` URI construction and decoding.
//!
//! Both directions were previously done by hand, wrongly, in several
//! places. Encoding was `format!("file://{}", path.display())`, which
//! produces an invalid URI for any path containing a space (or `#`, `?`,
//! or `%`) — and `lsp-types` parses URIs strictly, so a project under
//! `~/my project/` made the bundled servers reject the request outright.
//! Decoding was `uri.strip_prefix("file://").unwrap_or(uri)` in four
//! places, none of which percent-decoded, so a path a server handed back
//! encoded came out as a literal `/my%20project/a.rs` that does not exist.
//! That mattered most in `rename --apply`, the one command that writes to
//! disk: the undecoded path failed to open partway through the loop,
//! leaving the workspace half-renamed.

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use std::path::{Path, PathBuf};

/// Characters escaped in the path component of a `file:` URI.
///
/// Per RFC 3986 a path segment may contain unreserved characters, `sub-delims`,
/// `:` and `@`. `/` is kept literal because it is the separator. Everything
/// outside that — space, `#`, `?`, `%`, `"`, `<`, `>`, `\`, `` ` ``, `{`,
/// `}`, `|`, `^`, `[`, `]` — has to be escaped or the URI either fails to
/// parse or reparses as a different URI (`#` would start a fragment,
/// `?` a query).
const PATH_ESCAPE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Absolute filesystem path → `file:` URI.
pub fn from_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("file://{}", utf8_percent_encode(&raw, PATH_ESCAPE))
}

/// `file:` URI → filesystem path, percent-decoded.
///
/// Anything that isn't a `file:` URI is returned unchanged, so a server
/// that hands back a bare path (some do) still works.
pub fn to_path_string(uri: &str) -> String {
    let rest = match uri.strip_prefix("file://") {
        Some(r) => r,
        None => return uri.to_string(),
    };
    // An authority component ("file://host/path") is not something this
    // tool can open; only the empty authority ("file:///path") is local.
    // In practice servers always emit the empty form, so `rest` starts
    // with '/' and this is a no-op.
    percent_decode_str(rest).decode_utf8_lossy().into_owned()
}

/// `file:` URI → `PathBuf`.
pub fn to_path(uri: &str) -> PathBuf {
    PathBuf::from(to_path_string(uri))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_path_round_trips_unchanged() {
        let uri = from_path(Path::new("/home/u/repo/src/main.rs"));
        assert_eq!(uri, "file:///home/u/repo/src/main.rs");
        assert_eq!(to_path_string(&uri), "/home/u/repo/src/main.rs");
    }

    #[test]
    fn spaces_are_escaped_and_decoded_back() {
        // The case that made lsp-types reject the request and take the
        // bundled server down with it.
        let uri = from_path(Path::new("/home/u/my project/a.rs"));
        assert_eq!(uri, "file:///home/u/my%20project/a.rs");
        assert_eq!(to_path_string(&uri), "/home/u/my project/a.rs");
    }

    #[test]
    fn separators_stay_literal_so_the_path_structure_survives() {
        let uri = from_path(Path::new("/a/b/c.rs"));
        assert_eq!(uri.matches('/').count(), 5); // 2 from "//" + 3 separators
    }

    #[test]
    fn reserved_characters_that_would_reparse_are_escaped() {
        let uri = from_path(Path::new("/tmp/a#b?c[d].rs"));
        assert!(!uri.contains('#'), "unescaped # starts a fragment: {uri}");
        assert!(!uri.contains('?'), "unescaped ? starts a query: {uri}");
        assert_eq!(to_path_string(&uri), "/tmp/a#b?c[d].rs");
    }

    #[test]
    fn a_literal_percent_in_a_filename_survives_the_round_trip() {
        // Without escaping '%' this decodes to something else entirely.
        let uri = from_path(Path::new("/tmp/100%25.txt"));
        assert_eq!(to_path_string(&uri), "/tmp/100%25.txt");
    }

    #[test]
    fn non_ascii_paths_round_trip() {
        let uri = from_path(Path::new("/tmp/café/naïve.rs"));
        assert_eq!(to_path_string(&uri), "/tmp/café/naïve.rs");
    }

    #[test]
    fn decodes_encoding_this_tool_did_not_produce() {
        // rust-analyzer and typescript-language-server both percent-encode
        // what they hand back, regardless of the form we sent.
        assert_eq!(
            to_path_string("file:///repo/my%20project/b.rs"),
            "/repo/my project/b.rs"
        );
    }

    #[test]
    fn a_non_file_uri_is_passed_through_untouched() {
        assert_eq!(to_path_string("untitled:Untitled-1"), "untitled:Untitled-1");
        assert_eq!(to_path_string("/already/a/path"), "/already/a/path");
    }
}
