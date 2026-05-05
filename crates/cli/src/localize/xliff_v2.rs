//! XLIFF 2.0 parser — `<unit id="..."> <segment> <target>...</target>`.
//!
//! XLIFF 2.0 has been Angular's recommended translation format since v9 and
//! is the default emitted by `ng extract-i18n` with no `--format` flag. The
//! schema differs from 1.2 in three ways relevant here:
//!
//! - The translation unit element is `<unit>` (not `<trans-unit>`).
//! - Source and target text live inside one or more `<segment>` children
//!   rather than directly under the unit.
//! - The root carries `srcLang` / `trgLang` (camelCased) instead of 1.2's
//!   `source-language` / `target-language` attributes.
//!
//! Same hand-rolled cursor approach as the 1.2 parser — the schema is flat
//! enough that a full XML dependency would be overkill. When a unit has
//! multiple segments their `<target>` contents are concatenated in document
//! order, matching how `@angular/localize` reassembles a translated
//! message.

use super::{decode_xml_entities, extract_attr, TranslationMap};

/// Parse an XLIFF 2.0 document from a source string into a `{ id → target }`
/// map. Units without an `id` or without any `<target>` are silently
/// skipped — they can't be matched to a `$localize` message anyway.
pub fn parse_xliff_v2_str(xml: &str) -> TranslationMap {
    let mut out: TranslationMap = TranslationMap::new();
    let mut cursor = 0;
    while let Some(open_rel) = find_unit_open(&xml[cursor..]) {
        let open = cursor + open_rel;
        // Skip anything that turned out to be `<unitX...>` (rare, but cheap
        // to be defensive) — `find_unit_open` already requires a delimiter
        // after `unit`, so this is effectively just the close-bracket scan.
        let after_tag = match xml[open..].find('>') {
            Some(p) => open + p + 1,
            None => break,
        };
        let id = extract_attr(&xml[open..after_tag], "id").unwrap_or_default();
        let close_rel = match xml[after_tag..].find("</unit>") {
            Some(p) => p,
            None => break,
        };
        let body = &xml[after_tag..after_tag + close_rel];
        let target = collect_segment_targets(body);
        if !id.is_empty() && !target.is_empty() {
            out.insert(id, decode_xml_entities(&target));
        }
        cursor = after_tag + close_rel + "</unit>".len();
    }
    out
}

/// Locate the next opening `<unit ...>` tag in `xml`. Returns the byte
/// offset of the `<` delimiter. Excludes `<unitN>` style tag names — only
/// `<unit ` / `<unit>` / `<unit\t` / `<unit\n` / `<unit\r` count.
fn find_unit_open(xml: &str) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut cursor = 0;
    while let Some(rel) = xml[cursor..].find("<unit") {
        let pos = cursor + rel;
        let after = pos + "<unit".len();
        match bytes.get(after) {
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') => return Some(pos),
            _ => cursor = after,
        }
    }
    None
}

/// Walk every `<segment>` block inside `unit_body` and concatenate the
/// inner text of the `<target>` element each carries, in document order.
/// A unit with no segments at all (only `<ignorable>` runs) returns an
/// empty string; the caller treats that as "no translation available".
fn collect_segment_targets(unit_body: &str) -> String {
    let mut out = String::new();
    let mut cursor = 0;
    while let Some(open_rel) = unit_body[cursor..].find("<segment") {
        let open = cursor + open_rel;
        let after_tag = match unit_body[open..].find('>') {
            Some(p) => open + p + 1,
            None => break,
        };
        let close_rel = match unit_body[after_tag..].find("</segment>") {
            Some(p) => p,
            None => break,
        };
        let segment_body = &unit_body[after_tag..after_tag + close_rel];
        if let Some(target) = extract_target_text(segment_body) {
            out.push_str(&target);
        }
        cursor = after_tag + close_rel + "</segment>".len();
    }
    out
}

/// Extract the inner text of the first `<target>...</target>` element
/// inside `segment_body`. Returns `None` when a segment carries only a
/// `<source>` (an untranslated unit, common in freshly-extracted files).
fn extract_target_text(segment_body: &str) -> Option<String> {
    let open = segment_body.find("<target")?;
    let after_open = segment_body[open..].find('>')? + open + 1;
    let close = segment_body[after_open..].find("</target>")? + after_open;
    Some(segment_body[after_open..close].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_segment_unit() {
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
        let map = parse_xliff_v2_str(xml);
        assert_eq!(map.get("intro"), Some(&"Hallo".to_string()));
    }

    #[test]
    fn parses_multiple_units() {
        let xml = r#"<xliff version="2.0" srcLang="en">
  <file id="f1">
    <unit id="a"><segment><source>A</source><target>AA</target></segment></unit>
    <unit id="b"><segment><source>B</source><target>BB</target></segment></unit>
  </file>
</xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert_eq!(map.get("a"), Some(&"AA".to_string()));
        assert_eq!(map.get("b"), Some(&"BB".to_string()));
    }

    #[test]
    fn concatenates_multi_segment_target() {
        // Each segment's <target> text is trimmed (matching the 1.2 parser's
        // existing whitespace handling), then the trimmed runs are joined
        // in document order. Angular emits multi-segment units only when an
        // inline element splits the source, where the surrounding inline
        // markup carries the whitespace — so the joined text being run
        // together is the correct shape.
        let xml = r#"<xliff version="2.0">
  <file id="f1">
    <unit id="multi">
      <segment><source>One.</source><target>Eins.</target></segment>
      <segment><source>Two.</source><target>Zwei.</target></segment>
    </unit>
  </file>
</xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert_eq!(map.get("multi"), Some(&"Eins.Zwei.".to_string()));
    }

    #[test]
    fn skips_unit_with_only_source() {
        let xml = r#"<xliff version="2.0">
  <file id="f1">
    <unit id="untranslated">
      <segment><source>Pending</source></segment>
    </unit>
    <unit id="ok">
      <segment><source>OK</source><target>Bestätigt</target></segment>
    </unit>
  </file>
</xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert!(!map.contains_key("untranslated"));
        assert_eq!(map.get("ok"), Some(&"Bestätigt".to_string()));
    }

    #[test]
    fn decodes_xml_entities() {
        let xml = r#"<xliff version="2.0">
  <file id="f1">
    <unit id="entities">
      <segment><source>A &amp; B</source><target>X &amp; Y</target></segment>
    </unit>
  </file>
</xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert_eq!(map.get("entities"), Some(&"X & Y".to_string()));
    }

    #[test]
    fn ignores_notes_outside_segment() {
        let xml = r#"<xliff version="2.0">
  <file id="f1">
    <unit id="withnotes">
      <notes>
        <note category="description">A description</note>
      </notes>
      <segment><source>Src</source><target>Tgt</target></segment>
    </unit>
  </file>
</xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert_eq!(map.get("withnotes"), Some(&"Tgt".to_string()));
    }

    #[test]
    fn does_not_match_unit_lookalike() {
        // `<unitless>` should not be picked up by the unit scanner.
        let xml = r#"<xliff version="2.0">
  <file id="f1">
    <unitless/>
    <unit id="real"><segment><source>S</source><target>T</target></segment></unit>
  </file>
</xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert_eq!(map.get("real"), Some(&"T".to_string()));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn empty_file_returns_empty_map() {
        let xml = r#"<xliff version="2.0"><file id="f1"/></xliff>"#;
        let map = parse_xliff_v2_str(xml);
        assert!(map.is_empty());
    }
}
