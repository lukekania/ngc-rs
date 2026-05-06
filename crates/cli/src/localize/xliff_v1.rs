//! XLIFF 1.2 parser — `<trans-unit id="..."> <target>...</target>`.
//!
//! Hand-rolled cursor walk to avoid pulling a full XML parser into the CLI
//! for what is effectively a flat, well-known schema. Tolerant of attribute
//! order and whitespace; designed to handle both Angular's own
//! `ng extract-i18n` output and most third-party translation editors.

use super::{decode_xml_entities, extract_attr, extract_inner_text, TranslationMap};

/// Parse an XLIFF 1.2 document from a source string into a `{ id → target }`
/// map. Entries without an `id` or `<target>` are silently skipped — they
/// can't be matched to a `$localize` message anyway.
pub fn parse_xliff_str(xml: &str) -> TranslationMap {
    let mut out: TranslationMap = TranslationMap::new();
    // Cursor over `xml`; advance past each `<trans-unit id="...">` block,
    // grab the inner `<target>...</target>` text, restart from the closing
    // `</trans-unit>`.
    let mut cursor = 0;
    while let Some(open_rel) = xml[cursor..].find("<trans-unit") {
        let open = cursor + open_rel;
        let after_tag = match xml[open..].find('>') {
            Some(p) => open + p + 1,
            None => break,
        };
        let id = extract_attr(&xml[open..after_tag], "id").unwrap_or_default();
        let close_rel = match xml[after_tag..].find("</trans-unit>") {
            Some(p) => p,
            None => break,
        };
        let body = &xml[after_tag..after_tag + close_rel];
        if let Some(target) = extract_inner_text(body, "target") {
            if !id.is_empty() {
                out.insert(id, decode_xml_entities(&target));
            }
        }
        cursor = after_tag + close_rel + "</trans-unit>".len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xliff_trans_units() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<xliff version="1.2">
  <file>
    <body>
      <trans-unit id="intro" datatype="html">
        <source>Hello</source>
        <target>Hallo</target>
      </trans-unit>
      <trans-unit id="bye">
        <source>Bye</source>
        <target>Tsch&#252;ss</target>
      </trans-unit>
    </body>
  </file>
</xliff>"#;
        let map = parse_xliff_str(xml);
        assert_eq!(map.get("intro"), Some(&"Hallo".to_string()));
        assert_eq!(map.get("bye"), Some(&"Tsch&#252;ss".to_string()));
    }
}
