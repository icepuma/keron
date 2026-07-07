//! `file://` URI ⇄ filesystem path conversion.
//!
//! lsp-types 0.97 backs [`Uri`] with `fluent-uri`, which (unlike the
//! old `url` crate) ships no `from_file_path`/`to_file_path` helpers,
//! so this module hand-rolls the tiny subset the server needs:
//! percent-encoding of paths going out, percent-decoding of URIs
//! coming in, and the Windows drive-letter form (`file:///C:/…`).
//! Only `file` URIs convert; anything else returns `None` and the
//! server ignores the document.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::Uri;

/// Convert a `file://` URI to a filesystem path. Returns `None` for
/// non-`file` schemes, non-local authorities, undecodable escapes, or
/// non-UTF-8 path bytes.
#[must_use]
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = strip_scheme(s)?;
    // Split authority from the path at the first `/`.
    let (authority, raw_path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    if !(authority.is_empty() || authority.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    // file URIs from editors carry no query/fragment; tolerate and
    // drop them if one ever shows up.
    let raw_path = raw_path.split(['?', '#']).next().unwrap_or(raw_path);
    let decoded = percent_decode(raw_path)?;
    Some(decoded_to_path(&decoded))
}

/// Convert an absolute filesystem path to a `file://` URI. Returns
/// `None` for relative or non-UTF-8 paths.
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    if !path.is_absolute() {
        return None;
    }
    let s = path.to_str()?;
    let mut out = String::with_capacity(s.len() + 8);
    out.push_str("file://");
    if cfg!(windows) {
        // `fs::canonicalize` on Windows yields verbatim paths
        // (`\\?\C:\…`); editors speak `file:///C:/…`, so drop the
        // prefix or published-diagnostic URIs won't match the
        // client's document URIs.
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        let normalized = s.replace('\\', "/");
        // `C:/…` needs the extra root slash: `file:///C:/…`. UNC paths
        // (`//server/share`) already start with the slashes fluent-uri
        // expects after the empty authority.
        if !normalized.starts_with('/') {
            out.push('/');
        }
        percent_encode_into(&mut out, &normalized);
    } else {
        percent_encode_into(&mut out, s);
    }
    Uri::from_str(&out).ok()
}

fn strip_scheme(s: &str) -> Option<&str> {
    let (scheme, rest) = s.split_once("://")?;
    scheme.eq_ignore_ascii_case("file").then_some(rest)
}

/// Bytes that travel bare in a URI path: RFC 3986 unreserved plus the
/// pchar extras that matter for paths (`/`, `:`, `@`, and the
/// sub-delims that commonly appear in filenames).
const fn is_bare_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':' | b'@' | b'+')
}

fn percent_encode_into(out: &mut String, s: &str) {
    for &b in s.as_bytes() {
        if is_bare_path_byte(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from_digit(u32::from(b >> 4), 16).expect("nibble < 16"));
            out.push(
                char::from_digit(u32::from(b & 0xf), 16)
                    .expect("nibble < 16")
                    .to_ascii_uppercase(),
            );
        }
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).expect("two hex digits fit u8"));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn decoded_to_path(decoded: &str) -> PathBuf {
    if cfg!(windows) {
        // `/C:/…` (or the legacy `/C|/…`) → `C:/…`.
        let bytes = decoded.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && (bytes[2] == b':' || bytes[2] == b'|')
        {
            let mut s = String::with_capacity(decoded.len() - 1);
            s.push(bytes[1] as char);
            s.push(':');
            s.push_str(&decoded[3..]);
            return PathBuf::from(s);
        }
    }
    PathBuf::from(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).expect("valid test uri")
    }

    #[test]
    fn plain_unix_path_roundtrips() {
        let path = Path::new("/home/user/dots/main.keron");
        let u = path_to_uri(path).expect("uri");
        assert_eq!(u.as_str(), "file:///home/user/dots/main.keron");
        assert_eq!(uri_to_path(&u), Some(path.to_path_buf()));
    }

    #[test]
    fn spaces_and_unicode_percent_encode() {
        let path = Path::new("/home/user/my dots/héllo.keron");
        let u = path_to_uri(path).expect("uri");
        assert!(u.as_str().contains("my%20dots"), "got {}", u.as_str());
        assert_eq!(uri_to_path(&u), Some(path.to_path_buf()));
    }

    #[test]
    fn decodes_editor_style_encoded_colon() {
        // VS Code percent-encodes aggressively; a lowercase escape
        // must decode the same as uppercase.
        let u = uri("file:///home/a%2fb%20c");
        assert_eq!(uri_to_path(&u), Some(PathBuf::from("/home/a/b c")));
    }

    #[test]
    fn localhost_authority_is_accepted() {
        let u = uri("file://localhost/etc/hosts");
        assert_eq!(uri_to_path(&u), Some(PathBuf::from("/etc/hosts")));
    }

    #[test]
    fn remote_authority_is_rejected() {
        let u = uri("file://example.com/etc/hosts");
        assert_eq!(uri_to_path(&u), None);
    }

    #[test]
    fn non_file_scheme_is_rejected() {
        let u = uri("untitled:Untitled-1");
        assert_eq!(uri_to_path(&u), None);
        let u = uri("https://example.com/x.keron");
        assert_eq!(uri_to_path(&u), None);
    }

    #[test]
    fn relative_path_makes_no_uri() {
        assert_eq!(path_to_uri(Path::new("relative/x.keron")), None);
    }

    #[test]
    fn truncated_percent_escape_is_rejected() {
        // fluent-uri already rejects `%2` at parse time; the decoder
        // must also defend on its own for inputs that sneak past.
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("ok%GG"), None);
        assert_eq!(percent_decode("fine%20"), Some("fine ".to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_letter_roundtrips() {
        let path = Path::new(r"C:\Users\me\dots\main.keron");
        let u = path_to_uri(path).expect("uri");
        assert_eq!(u.as_str(), "file:///C:/Users/me/dots/main.keron");
        assert_eq!(
            uri_to_path(&u),
            Some(PathBuf::from("C:/Users/me/dots/main.keron"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_encoded_drive_colon_decodes() {
        let u = uri("file:///c%3A/Users/me/x.keron");
        assert_eq!(uri_to_path(&u), Some(PathBuf::from("c:/Users/me/x.keron")));
    }

    proptest! {
        #[test]
        fn unix_paths_roundtrip(segments in proptest::collection::vec("[^/\u{0}]{1,12}", 1..5)) {
            let path = PathBuf::from(format!("/{}", segments.join("/")));
            if let Some(u) = path_to_uri(&path) {
                prop_assert_eq!(uri_to_path(&u), Some(path));
            }
        }

        #[test]
        fn uri_to_path_never_panics(s in "file://[ -~]{0,40}") {
            if let Ok(u) = Uri::from_str(&s) {
                let _ = uri_to_path(&u);
            }
        }
    }
}
