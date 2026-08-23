//! Filename resolution and sanitization.
//!
//! Every rule here corresponds to a real vulnerability class in download tools:
//! path traversal, NTFS alternate data streams, extension spoofing via bidi overrides,
//! and Windows reserved device names.
//!
//! The final backstop lives in `resolve_destination`: whatever this module produces, the
//! canonicalized destination must still sit inside the configured directory, or we fail closed.

use percent_encoding::percent_decode_str;
use std::path::{Component, Path, PathBuf};

/// Longest filename we will produce, in UTF-16 code units (NTFS component limit is 255).
const MAX_STEM_UTF16: usize = 200;

/// Windows reserved device names. Reserved with *or without* an extension: `CON.txt` is
/// still `CON`, and creating it fails or opens the device.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FilenameError {
    #[error("filename escapes the destination directory")]
    Escapes,
    #[error("destination path is not valid unicode")]
    NotUnicode,
}

/// Sanitize an untrusted filename into something safe to create on Windows.
///
/// Always returns a usable name: unsafe input is repaired rather than rejected, because
/// refusing to download a file because a server sent an odd name is worse UX than renaming it.
/// The one thing this must never do is return something that can escape a directory.
pub fn sanitize(raw: &str) -> String {
    // 1. Take the last path component only. Strip both separators regardless of platform,
    //    because a server can send either and we always land on Windows.
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();

    // 2/3. Drop control characters and Unicode bidi overrides.
    //
    // U+202E RIGHT-TO-LEFT OVERRIDE is the classic extension spoof: "invoice\u{202E}xcod.exe"
    // renders to a human as "invoicexe.docx" while executing as .exe.
    let mut cleaned: String = last
        .chars()
        .filter(|&c| {
            !c.is_control()
                && !matches!(c,
                    '\u{202A}'..='\u{202E}'   // LRE RLE PDF LRO RLO
                    | '\u{2066}'..='\u{2069}' // LRI RLI FSI PDI
                    | '\u{200E}' | '\u{200F}' // LRM RLM
                    | '\u{FEFF}'              // zero-width no-break space
                )
        })
        // 4. Windows-invalid characters. ':' matters twice over: besides being invalid in a
        //    path, it opens an NTFS *alternate data stream*, so "report.pdf:payload.exe"
        //    would write a hidden executable stream alongside an innocuous-looking file.
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();

    // 2 (cont). "." and ".." are traversal, not names.
    if cleaned == "." || cleaned == ".." || cleaned.chars().all(|c| c == '.') {
        cleaned = String::new();
    }

    // 5. Windows silently strips trailing dots and spaces, so a name we recorded would not
    //    match the name on disk — which later breaks resume, delete and overwrite detection.
    let trimmed = cleaned.trim_end_matches([' ', '.']).trim_start();
    let mut name = trimmed.to_string();

    if name.is_empty() {
        name = "download".to_string();
    }

    // 6. Reserved device names, case-insensitively, ignoring any extension.
    {
        let stem = name.split('.').next().unwrap_or("");
        if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
            name.insert(0, '_');
        }
    }

    // 7. Truncate, preserving the extension so the file still opens with the right app.
    truncate_preserving_extension(&name)
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn truncate_by_utf16(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

fn truncate_preserving_extension(name: &str) -> String {
    if utf16_len(name) <= MAX_STEM_UTF16 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        // Only treat it as an extension if it is short enough to be one.
        Some((stem, ext)) if !ext.is_empty() && utf16_len(ext) <= 16 => {
            let budget = MAX_STEM_UTF16.saturating_sub(utf16_len(ext) + 1);
            format!("{}.{}", truncate_by_utf16(stem, budget), ext)
        }
        _ => truncate_by_utf16(name, MAX_STEM_UTF16),
    }
}

/// Resolve a filename from `Content-Disposition`, falling back to the URL path, then a
/// content-type guess. The result is always sanitized.
pub fn resolve(
    content_disposition: Option<&str>,
    final_url: &url::Url,
    content_type: Option<&str>,
) -> String {
    if let Some(cd) = content_disposition {
        if let Some(name) = parse_content_disposition(cd) {
            let s = sanitize(&name);
            if s != "download" {
                return s;
            }
        }
    }

    // URL path basename, percent-decoded.
    if let Some(seg) = final_url
        .path_segments()
        .and_then(|mut s| s.rfind(|s| !s.is_empty()))
    {
        let decoded = percent_decode_str(seg).decode_utf8_lossy().to_string();
        let s = sanitize(&decoded);
        if s != "download" {
            return s;
        }
    }

    match content_type.and_then(extension_for_mime) {
        Some(ext) => format!("download.{ext}"),
        None => "download".to_string(),
    }
}

/// Parse RFC 6266 `Content-Disposition`, preferring RFC 5987 `filename*` over `filename`.
pub fn parse_content_disposition(header: &str) -> Option<String> {
    let mut plain: Option<String> = None;

    for part in split_header_params(header) {
        // The disposition type itself ("attachment", "inline") carries no '=' — skip it
        // rather than aborting the whole parse.
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        if key == "filename*" {
            // ext-value: charset'language'percent-encoded-value
            let mut it = value.splitn(3, '\'');
            let charset = it.next().unwrap_or("");
            let _lang = it.next();
            if let Some(encoded) = it.next() {
                let decoded = percent_decode_str(encoded);
                let s = if charset.eq_ignore_ascii_case("utf-8") || charset.is_empty() {
                    decoded.decode_utf8_lossy().to_string()
                } else {
                    // ISO-8859-1 is the only other charset RFC 5987 permits.
                    decoded.map(|b| b as char).collect()
                };
                if !s.is_empty() {
                    return Some(s); // filename* wins outright
                }
            }
        } else if key == "filename" && plain.is_none() {
            let unquoted = value.trim_matches('"');
            if !unquoted.is_empty() {
                plain = Some(unquoted.replace("\\\"", "\""));
            }
        }
    }
    plain
}

/// Split on `;` while respecting quoted strings, so `filename="a;b.txt"` stays intact.
fn split_header_params(header: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for c in header.chars() {
        match c {
            '\\' if in_quotes && !escaped => {
                escaped = true;
                cur.push(c);
            }
            '"' if !escaped => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ';' if !in_quotes => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => {
                escaped = false;
                cur.push(c);
            }
        }
    }
    parts.push(cur);
    parts
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match base.as_str() {
        "application/zip" => "zip",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/x-tar" => "tar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msdownload" => "exe",
        "application/octet-stream" => "bin",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "video/x-matroska" => "mkv",
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        _ => return None,
    })
}

/// Join a sanitized filename onto a destination directory and prove the result stays inside it.
///
/// This is the final backstop: even if every rule above were bypassed, a path that escapes
/// `dir` is refused here. We compare lexically (after removing `.`/`..`) rather than with
/// `canonicalize`, because the destination file does not exist yet and canonicalizing a
/// non-existent path fails.
pub fn resolve_destination(dir: &Path, filename: &str) -> Result<PathBuf, FilenameError> {
    let safe = sanitize(filename);
    let candidate = dir.join(&safe);

    let normalized = normalize_lexically(&candidate);
    let base = normalize_lexically(dir);

    if !normalized.starts_with(&base) {
        return Err(FilenameError::Escapes);
    }
    // A sanitized name is exactly one component; anything else means a rule leaked.
    let rel = normalized
        .strip_prefix(&base)
        .map_err(|_| FilenameError::Escapes)?;
    if rel.components().count() != 1 {
        return Err(FilenameError::Escapes);
    }
    Ok(normalized)
}

/// Remove `.` and resolve `..` textually, without touching the filesystem.
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Pick a non-colliding name by inserting " (n)" before the extension, Explorer-style.
pub fn dedupe(dir: &Path, filename: &str) -> String {
    if !dir.join(filename).exists() {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (filename, None),
    };
    for n in 2..10_000 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{stem}-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_path_components() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("..\\..\\windows\\system32\\evil.dll"), "evil.dll");
        assert_eq!(sanitize("/absolute/path/file.zip"), "file.zip");
        assert_eq!(sanitize("C:\\Users\\x\\file.txt"), "file.txt");
    }

    #[test]
    fn traversal_names_never_survive() {
        assert_eq!(sanitize(".."), "download");
        assert_eq!(sanitize("."), "download");
        assert_eq!(sanitize("...."), "download");
        assert_eq!(sanitize("../.."), "download");
    }

    #[test]
    fn rejects_alternate_data_streams() {
        // The ADS case: "report.pdf:payload.exe" must not stay a stream reference.
        let s = sanitize("report.pdf:payload.exe");
        assert!(!s.contains(':'), "ADS separator survived: {s}");
        assert_eq!(s, "report.pdf_payload.exe");
        assert!(!sanitize("file.txt:Zone.Identifier").contains(':'));
    }

    #[test]
    fn strips_bidi_override_extension_spoof() {
        // Renders as "invoicexcod.exe" reversed -> looks like a .docx to a human.
        let spoof = "invoice\u{202E}exe.docx";
        let s = sanitize(spoof);
        assert!(!s.contains('\u{202E}'), "bidi override survived: {s:?}");
        assert_eq!(s, "invoiceexe.docx");
        for c in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{2066}', '\u{2069}', '\u{200E}',
        ] {
            assert!(!sanitize(&format!("a{c}b.txt")).contains(c));
        }
    }

    #[test]
    fn replaces_windows_invalid_characters() {
        assert_eq!(sanitize(r#"a<b>c:d"e|f?g*h.txt"#), "a_b_c_d_e_f_g_h.txt");
    }

    #[test]
    fn strips_control_characters() {
        assert_eq!(sanitize("file\u{0}\u{1}\u{1f}\u{7f}.txt"), "file.txt");
        assert_eq!(sanitize("line\nbreak.txt"), "linebreak.txt");
    }

    #[test]
    fn strips_trailing_dots_and_spaces() {
        // Windows silently trims these, so we must too or our record won't match disk.
        assert_eq!(sanitize("file.txt   "), "file.txt");
        assert_eq!(sanitize("file.txt..."), "file.txt");
        assert_eq!(sanitize("file.txt . . "), "file.txt");
    }

    #[test]
    fn escapes_reserved_device_names() {
        for n in ["CON", "con", "PRN", "aux", "NUL", "COM1", "lpt9"] {
            let s = sanitize(n);
            assert!(s.starts_with('_'), "{n} -> {s}");
        }
        // Reserved with an extension is still reserved.
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("com4.log"), "_com4.log");
        // But a name that merely starts with those letters is fine.
        assert_eq!(sanitize("console.log"), "console.log");
        assert_eq!(sanitize("connection.txt"), "connection.txt");
    }

    #[test]
    fn empty_and_whitespace_get_a_fallback() {
        assert_eq!(sanitize(""), "download");
        assert_eq!(sanitize("   "), "download");
        assert_eq!(sanitize("///"), "download");
    }

    #[test]
    fn truncates_long_names_but_keeps_the_extension() {
        let long = format!("{}.zip", "a".repeat(500));
        let s = sanitize(&long);
        assert!(utf16_len(&s) <= MAX_STEM_UTF16, "len {}", utf16_len(&s));
        assert!(s.ends_with(".zip"), "extension lost: {s}");
    }

    #[test]
    fn truncation_counts_utf16_units_not_bytes() {
        // Emoji are 2 UTF-16 units each; NTFS counts UTF-16.
        let long = "😀".repeat(300);
        let s = sanitize(&long);
        assert!(utf16_len(&s) <= MAX_STEM_UTF16);
        assert!(!s.is_empty());
    }

    #[test]
    fn parses_content_disposition_plain() {
        assert_eq!(
            parse_content_disposition(r#"attachment; filename="my file.zip""#).as_deref(),
            Some("my file.zip")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=simple.txt").as_deref(),
            Some("simple.txt")
        );
    }

    #[test]
    fn content_disposition_star_wins_over_plain() {
        let h = r#"attachment; filename="fallback.txt"; filename*=UTF-8''caf%C3%A9%20r%C3%A9sum%C3%A9.pdf"#;
        assert_eq!(
            parse_content_disposition(h).as_deref(),
            Some("café résumé.pdf")
        );
    }

    #[test]
    fn content_disposition_handles_semicolon_inside_quotes() {
        let h = r#"attachment; filename="a;b.txt""#;
        assert_eq!(parse_content_disposition(h).as_deref(), Some("a;b.txt"));
    }

    #[test]
    fn content_disposition_traversal_is_still_sanitized() {
        let url = url::Url::parse("https://example.com/x").unwrap();
        let name = resolve(Some(r#"attachment; filename="../../evil.exe""#), &url, None);
        assert_eq!(name, "evil.exe");
    }

    #[test]
    fn resolve_falls_back_through_the_chain() {
        let url = url::Url::parse("https://example.com/files/movie.mkv?token=abc").unwrap();
        assert_eq!(resolve(None, &url, None), "movie.mkv");

        let bare = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(
            resolve(None, &bare, Some("application/pdf")),
            "download.pdf"
        );
        assert_eq!(resolve(None, &bare, None), "download");
    }

    #[test]
    fn resolve_percent_decodes_url_segment() {
        let url = url::Url::parse("https://example.com/my%20file%20(1).zip").unwrap();
        assert_eq!(resolve(None, &url, None), "my file (1).zip");
    }

    #[test]
    fn destination_containment_is_enforced() {
        let dir = Path::new("/downloads");
        assert_eq!(
            resolve_destination(dir, "file.zip").unwrap(),
            PathBuf::from("/downloads/file.zip")
        );
        // Traversal is neutralized by sanitize, so this lands inside the directory.
        assert_eq!(
            resolve_destination(dir, "../../etc/passwd").unwrap(),
            PathBuf::from("/downloads/passwd")
        );
    }

    #[test]
    fn destination_never_escapes_for_any_input() {
        let dir = Path::new("/downloads");
        for raw in [
            "../../etc/passwd",
            "..\\..\\windows\\evil.dll",
            "/etc/shadow",
            "....//....//x",
            "a/../../../b.txt",
            "\u{202E}gpj.exe",
            "CON",
            "",
            "   ",
            ".",
            "..",
            "file.txt:stream",
        ] {
            let got = resolve_destination(dir, raw).expect("must resolve, not error");
            assert!(got.starts_with(dir), "{raw:?} escaped to {got:?}");
            assert_eq!(
                got.components().count(),
                dir.components().count() + 1,
                "{raw:?} produced nested path {got:?}"
            );
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_sanitize_output_is_always_a_single_safe_component(raw in ".{0,200}") {
            let s = sanitize(&raw);
            proptest::prop_assert!(!s.is_empty());
            proptest::prop_assert!(!s.contains('/'), "slash in {:?}", s);
            proptest::prop_assert!(!s.contains('\\'), "backslash in {:?}", s);
            proptest::prop_assert!(!s.contains(':'), "ADS colon in {:?}", s);
            proptest::prop_assert!(s != "." && s != "..");
            proptest::prop_assert!(!s.ends_with(' ') && !s.ends_with('.'));
            proptest::prop_assert!(!s.chars().any(|c| c.is_control()));
            proptest::prop_assert!(utf16_len(&s) <= MAX_STEM_UTF16);
        }

        /// The invariant that actually matters: nothing escapes the destination directory.
        #[test]
        fn prop_destination_is_always_contained(raw in ".{0,200}") {
            let dir = Path::new("/downloads");
            let got = resolve_destination(dir, &raw).expect("sanitize repairs rather than rejects");
            proptest::prop_assert!(got.starts_with(dir));
            proptest::prop_assert_eq!(got.components().count(), dir.components().count() + 1);
        }
    }
}
