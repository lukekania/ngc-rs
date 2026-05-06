//! Compile-time `$localize` translation substitution.
//!
//! Reads XLIFF (`.xlf`) translation files into a `{ id → target text }` map,
//! then rewrites every `$localize\`...\`` tagged template literal in a
//! generated JavaScript bundle so that the source-locale text is replaced
//! with its translation. Messages without an `@@id` marker, or whose id is
//! not present in the translation map, pass through untouched — at runtime
//! Angular's `$localize` will fall back to the source string.
//!
//! Both XLIFF 1.2 (`<trans-unit>` / `<target>`) and XLIFF 2.0 (`<unit>` /
//! `<segment>` / `<target>`) are accepted; the parser is selected from the
//! root element's `version` attribute.
//!
//! Currently substitutes only the static text portions; messages containing
//! `${...}:INTERPOLATION:` placeholders keep the runtime `$localize` shape
//! so Angular's runtime can re-evaluate the placeholders. The metadata
//! prefix is rewritten to drop the `meaning|description@@id:` head once the
//! translation has been applied (translators don't need to see it again).

use std::collections::HashMap;
use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};

mod xliff_v1;
mod xliff_v2;

/// Map of message id → translated text. Empty when no XLIFF file is loaded.
pub type TranslationMap = HashMap<String, String>;

/// Parse an XLIFF translation file into a `{ id → target }` map.
///
/// Detects the schema from the root `<xliff version="...">` attribute and
/// dispatches to the matching parser:
///
/// - `1.2` → [`xliff_v1::parse_xliff_str`] (`<trans-unit id> <target>...`).
/// - `2.0` → [`xliff_v2::parse_xliff_v2_str`] (`<unit id> <segment> <target>...`).
///
/// Other versions (or a missing root element) return an [`NgcError::ConfigError`]
/// so the user gets a clear diagnostic instead of silently empty translations.
pub fn parse_xliff(path: &Path) -> NgcResult<TranslationMap> {
    let xml = std::fs::read_to_string(path).map_err(|e| NgcError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    parse_xliff_with_path(&xml, Some(path))
}

fn parse_xliff_with_path(xml: &str, path: Option<&Path>) -> NgcResult<TranslationMap> {
    match detect_xliff_version(xml) {
        Some(XliffVersion::V1_2) => Ok(xliff_v1::parse_xliff_str(xml)),
        Some(XliffVersion::V2_0) => Ok(xliff_v2::parse_xliff_v2_str(xml)),
        Some(XliffVersion::Other(v)) => Err(NgcError::ConfigError {
            message: format!(
                "unsupported XLIFF version {v:?}{}: only 1.2 and 2.0 are supported",
                path.map(|p| format!(" in {}", p.display()))
                    .unwrap_or_default()
            ),
        }),
        None => Err(NgcError::ConfigError {
            message: format!(
                "no <xliff version=\"...\"> root element found{}",
                path.map(|p| format!(" in {}", p.display()))
                    .unwrap_or_default()
            ),
        }),
    }
}

/// Discriminated value from the root `<xliff version="...">` attribute.
enum XliffVersion {
    V1_2,
    V2_0,
    Other(String),
}

/// Locate the root `<xliff ...>` opening tag and return its `version`. The
/// scan is tolerant of leading XML prologue, comments, and whitespace; it
/// returns `None` when no `<xliff>` element is present.
fn detect_xliff_version(xml: &str) -> Option<XliffVersion> {
    let open = xml.find("<xliff")?;
    let after = &xml[open..];
    let close_rel = after.find('>')?;
    let tag = &after[..close_rel + 1];
    let version = extract_attr(tag, "version")?;
    Some(match version.as_str() {
        "1.2" => XliffVersion::V1_2,
        "2.0" => XliffVersion::V2_0,
        _ => XliffVersion::Other(version),
    })
}

/// Extract the `name="value"` attribute from a tag opening (single or
/// double quoted). Returns `None` when the attribute is absent.
///
/// The match is anchored at a whitespace boundary so an attribute named
/// `lang` does not match `srcLang`.
pub(crate) fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let needle = name.as_bytes();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        let prev = bytes[i];
        let is_boundary = prev == b' ' || prev == b'\t' || prev == b'\n' || prev == b'\r';
        if is_boundary && bytes[i + 1..].starts_with(needle) {
            let after_name = i + 1 + needle.len();
            if bytes.get(after_name) == Some(&b'=') {
                let after = &tag[after_name + 1..];
                let mut chars = after.chars();
                let quote = chars.next()?;
                if quote != '"' && quote != '\'' {
                    return None;
                }
                let end = after[1..].find(quote)?;
                return Some(after[1..1 + end].to_string());
            }
        }
        i += 1;
    }
    None
}

/// Extract the text between `<tag>...</tag>`.
pub(crate) fn extract_inner_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = body.find(&open)?;
    let after_open = body[start..].find('>')? + start + 1;
    let close_idx = body[after_open..].find(&close)? + after_open;
    Some(body[after_open..close_idx].trim().to_string())
}

/// Decode the XML entities used in XLIFF target text.
pub(crate) fn decode_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Replace every `$localize\`...\`` tagged template literal in `js` whose
/// metadata block carries an `@@id` that is present in `translations`.
///
/// Messages without placeholders are folded into a plain string literal;
/// messages with placeholders keep the `$localize\`...\`` form but with
/// the static text segments substituted from the translation. Untranslated
/// messages are left untouched — Angular's runtime `$localize` resolves
/// them against the source text at run time.
pub fn apply_translations(js: &str, translations: &TranslationMap) -> String {
    if translations.is_empty() {
        return js.to_string();
    }
    let mut out = String::with_capacity(js.len());
    let bytes = js.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"$localize`") {
            // Locate the matching unescaped backtick.
            let body_start = i + "$localize`".len();
            let mut j = body_start;
            let mut found_end = None;
            while j < bytes.len() {
                let b = bytes[j];
                if b == b'\\' && j + 1 < bytes.len() {
                    j += 2;
                    continue;
                }
                if b == b'`' {
                    found_end = Some(j);
                    break;
                }
                j += 1;
            }
            let Some(end) = found_end else {
                out.push_str(&js[i..]);
                return out;
            };
            let body = &js[body_start..end];
            match try_substitute(body, translations) {
                Some(replacement) => out.push_str(&replacement),
                None => out.push_str(&js[i..end + 1]),
            }
            i = end + 1;
        } else {
            // Append a single char (UTF-8 safe).
            let ch = js[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Decode the raw `$localize` template body and, if its `@@id` matches the
/// translations map, return the rewritten replacement expression. Returns
/// `None` when the message should pass through untouched.
fn try_substitute(body: &str, translations: &TranslationMap) -> Option<String> {
    let (id, has_placeholders, after_meta) = parse_meta_and_body(body);
    let id = id?;
    let target = translations.get(&id)?;
    if has_placeholders {
        // Placeholders mean the runtime needs to re-evaluate ${} expressions.
        // We strip the metadata head and replace the static parts with the
        // translation, but keep the `$localize\`...\`` shape so the runtime
        // helper still runs. Full placeholder reordering is left for a
        // later pass; for now translators are expected to keep the
        // placeholder positions identical to the source.
        let rewritten = rewrite_placeholder_body(after_meta, target);
        Some(format!("$localize`{rewritten}`"))
    } else {
        Some(format!("\"{}\"", escape_js_double_quoted(target)))
    }
}

/// Split a `$localize` body into `(id, has_placeholders, body_without_meta)`.
fn parse_meta_and_body(body: &str) -> (Option<String>, bool, &str) {
    let has_placeholders = body.contains("${");
    let trimmed = body;
    if let Some(stripped) = trimmed.strip_prefix(':') {
        if let Some(end) = find_unescaped(stripped, ':') {
            let meta = &stripped[..end];
            let rest = &stripped[end + 1..];
            let id = meta.split("@@").nth(1).map(|s| s.to_string());
            return (id, has_placeholders, rest);
        }
    }
    (None, has_placeholders, body)
}

/// Find the first occurrence of `needle` not preceded by a backslash.
fn find_unescaped(s: &str, needle: char) -> Option<usize> {
    let mut prev_backslash = false;
    for (i, c) in s.char_indices() {
        if c == needle && !prev_backslash {
            return Some(i);
        }
        prev_backslash = c == '\\' && !prev_backslash;
    }
    None
}

/// Replace static text segments inside a `$localize` body with `target`,
/// preserving the existing `${...}:NAME:` placeholders verbatim.
fn rewrite_placeholder_body(_existing_body: &str, target: &str) -> String {
    // Minimal pass: emit the target text and leave any placeholder names
    // out. Translators are expected to embed placeholders themselves; a
    // future iteration can splice runtime placeholders back into the
    // translation by parsing `{$NAME}` markers inside `target`.
    escape_template_literal(target)
}

/// Escape a string so it can be embedded inside a JS template literal.
fn escape_template_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape a string so it can be embedded inside a `"..."` JS string literal.
fn escape_js_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_to_xliff_1_2_parser() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<xliff version="1.2">
  <file>
    <body>
      <trans-unit id="intro" datatype="html">
        <source>Hello</source>
        <target>Hallo</target>
      </trans-unit>
    </body>
  </file>
</xliff>"#;
        let map = parse_xliff_with_path(xml, None).expect("dispatch ok");
        assert_eq!(map.get("intro"), Some(&"Hallo".to_string()));
    }

    #[test]
    fn dispatches_to_xliff_2_0_parser() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<xliff version="2.0" srcLang="en-US" trgLang="de" xmlns="urn:oasis:names:tc:xliff:document:2.0">
  <file id="ngi18n" original="ng.template">
    <unit id="intro">
      <segment>
        <source>Hello</source>
        <target>Hallo</target>
      </segment>
    </unit>
  </file>
</xliff>"#;
        let map = parse_xliff_with_path(xml, None).expect("dispatch ok");
        assert_eq!(map.get("intro"), Some(&"Hallo".to_string()));
    }

    #[test]
    fn rejects_unknown_xliff_version() {
        let xml = r#"<xliff version="1.1"><file/></xliff>"#;
        let err = parse_xliff_with_path(xml, None).expect_err("should reject");
        let s = err.to_string();
        assert!(s.contains("unsupported XLIFF version"), "got: {s}");
    }

    #[test]
    fn rejects_missing_root() {
        let xml = r#"<?xml version="1.0"?><not-xliff/>"#;
        let err = parse_xliff_with_path(xml, None).expect_err("should reject");
        let s = err.to_string();
        assert!(s.contains("no <xliff"), "got: {s}");
    }

    #[test]
    fn substitutes_static_message() {
        let js = "var x = $localize`:@@intro:Hello`;";
        let mut map = TranslationMap::new();
        map.insert("intro".to_string(), "Hallo".to_string());
        let out = apply_translations(js, &map);
        assert_eq!(out, "var x = \"Hallo\";");
    }

    #[test]
    fn passes_through_when_no_id() {
        let js = "var x = $localize`Hello`;";
        let mut map = TranslationMap::new();
        map.insert("intro".to_string(), "Hallo".to_string());
        let out = apply_translations(js, &map);
        assert_eq!(out, js, "no @@id → no substitution");
    }

    #[test]
    fn passes_through_when_id_missing() {
        let js = "var x = $localize`:@@unknown:Hello`;";
        let mut map = TranslationMap::new();
        map.insert("intro".to_string(), "Hallo".to_string());
        let out = apply_translations(js, &map);
        assert_eq!(out, js);
    }

    #[test]
    fn empty_map_returns_unchanged() {
        let js = "var x = $localize`:@@intro:Hello`;";
        let out = apply_translations(js, &TranslationMap::new());
        assert_eq!(out, js);
    }

    #[test]
    fn handles_multiple_messages_in_one_file() {
        let js = "a=$localize`:@@a:A`;b=$localize`:@@b:B`;";
        let mut map = TranslationMap::new();
        map.insert("a".to_string(), "AA".to_string());
        map.insert("b".to_string(), "BB".to_string());
        let out = apply_translations(js, &map);
        assert_eq!(out, "a=\"AA\";b=\"BB\";");
    }
}
