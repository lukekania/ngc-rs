//! Helpers for compiling Angular's i18n (`i18n` attribute, `$localize`, ICU)
//! into Ivy runtime instructions.
//!
//! Angular's i18n pipeline represents translatable messages as
//! `$localize`-tagged template literals. At compile time the template's
//! translatable elements are lowered into:
//!
//! 1. A consts-array entry containing the `$localize` message (optionally
//!    prefixed by `:meaning|description@@id:` metadata).
//! 2. Creation-block instructions (`ɵɵi18n`, `ɵɵi18nAttributes`) that bind
//!    the message to an element or attribute.
//! 3. Update-block instructions (`ɵɵi18nExp`, `ɵɵi18nApply`) that feed
//!    interpolation values into the message at change-detection time.
//!
//! The helpers here produce the const-array expression and placeholder list;
//! the caller wires the returned pieces into the codegen state.

use crate::ast::{I18nMeta, InterpolationNode, TemplateNode, TextNode};

/// A compiled i18n message ready to be registered in the consts array.
#[derive(Debug, Clone)]
pub(crate) struct CompiledI18nMessage {
    /// The `$localize\`...\`` tagged template literal, as source text.
    pub localize_expr: String,
    /// The ordered interpolation expressions the runtime must evaluate —
    /// one `ɵɵi18nExp` call per entry, in order, followed by a single
    /// `ɵɵi18nApply`.
    pub interpolations: Vec<String>,
}

/// Build the translator-visible metadata prefix (`meaning|description@@id`).
/// Returns an empty string when all metadata fields are `None`.
fn format_meta(meta: &I18nMeta) -> String {
    let mut out = String::new();
    let has_head = meta.meaning.is_some() || meta.description.is_some();
    if let Some(m) = &meta.meaning {
        out.push_str(&escape_meta_segment(m));
    }
    if has_head && (meta.description.is_some() || meta.meaning.is_some()) {
        // Always emit the `|` separator when any head segment is present so
        // translators see the pipe even if only one side is supplied — the
        // same shape Angular's compiler produces.
        if meta.description.is_some() || meta.meaning.is_some() {
            out.push('|');
        }
    }
    if let Some(d) = &meta.description {
        out.push_str(&escape_meta_segment(d));
    }
    if let Some(id) = &meta.id {
        out.push_str("@@");
        out.push_str(&escape_meta_segment(id));
    }
    out
}

/// Escape a metadata segment so the `:` delimiter at the end of the prefix
/// remains unambiguous.
fn escape_meta_segment(raw: &str) -> String {
    raw.replace(':', "\\:")
}

/// Escape a char inside a JS template literal body.
fn escape_template_char(c: char, out: &mut String) {
    match c {
        '`' => out.push_str("\\`"),
        '\\' => out.push_str("\\\\"),
        '$' => out.push_str("\\$"),
        _ => out.push(c),
    }
}

/// Append an escaped literal text run to a template literal body.
fn append_template_text(body: &mut String, text: &str) {
    for c in text.chars() {
        escape_template_char(c, body);
    }
}

/// Build a `$localize` tagged template literal from a sequence of
/// translatable children and (optional) metadata.
///
/// Interpolations become placeholder expressions of the form
/// `${"\u{FFFD}N\u{FFFD}"}:INTERPOLATION:` — Angular's runtime format, where
/// `\u{FFFD}N\u{FFFD}` is the ordinal placeholder index. The raw
/// interpolation expressions are returned so the caller can emit
/// `ɵɵi18nExp` / `ɵɵi18nApply` in the update block.
pub(crate) fn compile_message(children: &[TemplateNode], meta: &I18nMeta) -> CompiledI18nMessage {
    let mut body = String::new();
    let meta_prefix = format_meta(meta);
    if !meta_prefix.is_empty() {
        body.push(':');
        body.push_str(&meta_prefix);
        body.push(':');
    }
    let mut interpolations: Vec<String> = Vec::new();
    walk_children(children, &mut body, &mut interpolations);
    CompiledI18nMessage {
        localize_expr: format!("$localize`{}`", body),
        interpolations,
    }
}

fn walk_children(children: &[TemplateNode], body: &mut String, interpolations: &mut Vec<String>) {
    for child in children {
        match child {
            TemplateNode::Text(TextNode { value }) => {
                append_template_text(body, value);
            }
            TemplateNode::Interpolation(InterpolationNode { expression, .. }) => {
                let idx = interpolations.len();
                interpolations.push(expression.clone());
                body.push_str(&format!(
                    "${{\"\\u{{FFFD}}{idx}\\u{{FFFD}}\"}}:INTERPOLATION:"
                ));
            }
            TemplateNode::Element(el) => {
                // Nested tag: emit a placeholder around its contents so the
                // translation can reposition or preserve formatting markup.
                // The runtime links this to the actual DOM via the template;
                // at extraction time the tag appears as `{$START_TAG_FOO}`.
                let tag_upper = el.tag.to_ascii_uppercase().replace('-', "_");
                body.push_str(&format!(
                    "${{\"\\u{{FFFD}}#{idx}\\u{{FFFD}}\"}}:START_TAG_{tag_upper}:",
                    idx = interpolations.len()
                ));
                interpolations.push(format!("/* element {} */", el.tag));
                walk_children(&el.children, body, interpolations);
                body.push_str(&format!(
                    "${{\"\\u{{FFFD}}/#{idx}\\u{{FFFD}}\"}}:CLOSE_TAG_{tag_upper}:",
                    idx = interpolations.len() - 1
                ));
            }
            TemplateNode::IcuExpression(icu) => {
                // Inline the raw ICU message — Angular's translator pipeline
                // understands the `{expr, plural, ...}` shape verbatim inside
                // a `$localize` literal.
                body.push('{');
                append_template_text(body, &icu.switch_expression);
                body.push_str(", ");
                body.push_str(match icu.category {
                    crate::ast::IcuCategory::Plural => "plural",
                    crate::ast::IcuCategory::Select => "select",
                    crate::ast::IcuCategory::SelectOrdinal => "selectordinal",
                });
                body.push_str(", ");
                for case in &icu.cases {
                    append_template_text(body, &case.key);
                    body.push_str(" {");
                    append_template_text(body, &case.body);
                    body.push_str("} ");
                }
                body.push('}');
            }
            _ => {
                // Other control-flow blocks inside an i18n region are
                // left untouched for now — a future pass can expand `@if`
                // /`@for` children into their own sub-messages.
            }
        }
    }
}

/// Build a `$localize` message for a single static attribute value.
/// The attribute carries no interpolations at this point; interpolation
/// inside `i18n-<attr>` bindings will be added alongside property-binding
/// support in a later iteration.
pub(crate) fn compile_attribute_message(value: &str, meta: &I18nMeta) -> CompiledI18nMessage {
    let mut body = String::new();
    let meta_prefix = format_meta(meta);
    if !meta_prefix.is_empty() {
        body.push(':');
        body.push_str(&meta_prefix);
        body.push(':');
    }
    append_template_text(&mut body, value);
    CompiledI18nMessage {
        localize_expr: format!("$localize`{}`", body),
        interpolations: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{InterpolationNode, TextNode};

    #[test]
    fn compiles_static_message() {
        let children = vec![TemplateNode::Text(TextNode {
            value: "Hello".to_string(),
        })];
        let msg = compile_message(&children, &I18nMeta::default());
        assert_eq!(msg.localize_expr, "$localize`Hello`");
        assert!(msg.interpolations.is_empty());
    }

    #[test]
    fn compiles_meta_prefix() {
        let children = vec![TemplateNode::Text(TextNode {
            value: "Hi".to_string(),
        })];
        let meta = I18nMeta {
            id: Some("intro".to_string()),
            meaning: Some("greeting".to_string()),
            description: Some("welcome".to_string()),
        };
        let msg = compile_message(&children, &meta);
        assert_eq!(msg.localize_expr, "$localize`:greeting|welcome@@intro:Hi`");
    }

    #[test]
    fn compiles_interpolation_placeholders() {
        let children = vec![
            TemplateNode::Text(TextNode {
                value: "Hello, ".to_string(),
            }),
            TemplateNode::Interpolation(InterpolationNode {
                expression: "name".to_string(),
                pipes: Vec::new(),
            }),
            TemplateNode::Text(TextNode {
                value: "!".to_string(),
            }),
        ];
        let msg = compile_message(&children, &I18nMeta::default());
        assert_eq!(msg.interpolations, vec!["name".to_string()]);
        assert!(msg.localize_expr.contains(":INTERPOLATION:"));
        assert!(msg.localize_expr.contains("\\u{FFFD}0\\u{FFFD}"));
    }

    #[test]
    fn escapes_template_backticks() {
        let children = vec![TemplateNode::Text(TextNode {
            value: "a`b$c\\d".to_string(),
        })];
        let msg = compile_message(&children, &I18nMeta::default());
        assert!(msg.localize_expr.contains("a\\`b\\$c\\\\d"));
    }

    #[test]
    fn compiles_attribute_message() {
        let meta = I18nMeta {
            id: Some("tip".to_string()),
            ..Default::default()
        };
        let msg = compile_attribute_message("Tooltip", &meta);
        assert_eq!(msg.localize_expr, "$localize`:@@tip:Tooltip`");
    }
}
