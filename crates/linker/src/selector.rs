//! Parse CSS selector strings into Angular's internal selector array format.
//!
//! Angular selectors use a nested array format: `[['tag', 'attr', 'value', ...], ...]`
//! where multiple top-level arrays represent comma-separated alternatives.
//!
//! ## Examples
//! - `"my-comp"` → `[["my-comp"]]`
//! - `"[myDir]"` → `[["", "myDir", ""]]`
//! - `"[attr=value]"` → `[["", "attr", "value"]]`
//! - `"my-comp, other"` → `[["my-comp"], ["other"]]`
//! - `".my-class"` → `[["", 8, "my-class"]]`

/// Parse a CSS selector string into Angular's selector array format.
///
/// Returns a JavaScript source string like `[['my-comp']]`.
pub fn parse_selector(selector: &str) -> String {
    let alternatives: Vec<&str> = selector.split(',').map(|s| s.trim()).collect();
    let mut parts = Vec::new();

    for alt in alternatives {
        parts.push(parse_single_selector(alt));
    }

    format!("[{}]", parts.join(", "))
}

/// Parse a single selector (no commas) into its array representation.
fn parse_single_selector(selector: &str) -> String {
    let selector = selector.trim();

    if selector.is_empty() {
        return "['']".to_string();
    }

    let mut items: Vec<String> = Vec::new();
    let mut tag = String::new();
    let mut i = 0;
    let chars: Vec<char> = selector.chars().collect();

    // Parse tag name (if present at the start)
    while i < chars.len() && chars[i] != '[' && chars[i] != '.' && chars[i] != ':' {
        tag.push(chars[i]);
        i += 1;
    }

    items.push(format!("'{}'", tag.trim()));

    // Parse remaining parts (attributes, classes)
    while i < chars.len() {
        match chars[i] {
            '[' => {
                // Attribute selector: [name] or [name=value]
                i += 1;
                let mut attr_name = String::new();
                let mut attr_value = String::new();
                let mut has_value = false;

                while i < chars.len() && chars[i] != ']' {
                    if chars[i] == '=' {
                        has_value = true;
                        i += 1;
                        // Skip optional quotes
                        if i < chars.len() && (chars[i] == '\'' || chars[i] == '"') {
                            let quote = chars[i];
                            i += 1;
                            while i < chars.len() && chars[i] != quote {
                                attr_value.push(chars[i]);
                                i += 1;
                            }
                            if i < chars.len() {
                                i += 1; // skip closing quote
                            }
                        } else {
                            while i < chars.len() && chars[i] != ']' {
                                attr_value.push(chars[i]);
                                i += 1;
                            }
                        }
                    } else {
                        attr_name.push(chars[i]);
                        i += 1;
                    }
                }
                if i < chars.len() {
                    i += 1; // skip ']'
                }

                items.push(format!("'{}'", attr_name.trim()));
                items.push(format!(
                    "'{}'",
                    if has_value { attr_value.trim() } else { "" }
                ));
            }
            '.' => {
                // Class selector
                i += 1;
                let mut class_name = String::new();
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' && chars[i] != ':' {
                    class_name.push(chars[i]);
                    i += 1;
                }
                // 8 = SelectorFlags.CLASS
                items.push("8".to_string());
                items.push(format!("'{}'", class_name.trim()));
            }
            ':' => {
                // Check for :not() pseudo-selector
                let remaining: String = chars[i..].iter().collect();
                if remaining.starts_with(":not(") {
                    i += 5; // skip ":not("
                            // Parse the inner selector parts until ')'
                    while i < chars.len() && chars[i] != ')' {
                        if chars[i] == '[' {
                            // :not([attr]) → emit SelectorFlags.NOT (4) + attr + value
                            i += 1;
                            let mut attr_name = String::new();
                            let mut attr_value = String::new();
                            let mut has_value = false;
                            while i < chars.len() && chars[i] != ']' {
                                if chars[i] == '=' {
                                    has_value = true;
                                    i += 1;
                                    while i < chars.len() && chars[i] != ']' {
                                        attr_value.push(chars[i]);
                                        i += 1;
                                    }
                                } else {
                                    attr_name.push(chars[i]);
                                    i += 1;
                                }
                            }
                            if i < chars.len() {
                                i += 1; // skip ']'
                            }
                            // 4 = SelectorFlags.NOT — Angular uses this in v21+,
                            // but some versions use 3 for NOT in selector context
                            // (distinct from AttributeMarker.Bindings which is also 3
                            // but used in a different array).  Angular 21 uses value 3.
                            items.push("3".to_string());
                            items.push(format!("'{}'", attr_name.trim()));
                            items.push(format!(
                                "'{}'",
                                if has_value { attr_value.trim() } else { "" }
                            ));
                        } else {
                            i += 1;
                        }
                    }
                    if i < chars.len() {
                        i += 1; // skip ')'
                    }
                } else {
                    // Other pseudo-selectors — skip
                    i += 1;
                    while i < chars.len()
                        && chars[i] != ' '
                        && chars[i] != '['
                        && chars[i] != '.'
                        && chars[i] != ':'
                    {
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_element_selector() {
        assert_eq!(parse_selector("my-comp"), "[['my-comp']]");
    }

    #[test]
    fn test_attribute_selector() {
        assert_eq!(parse_selector("[myDir]"), "[['', 'myDir', '']]");
    }

    #[test]
    fn test_attribute_with_value() {
        assert_eq!(parse_selector("[attr=value]"), "[['', 'attr', 'value']]");
    }

    #[test]
    fn test_multiple_selectors() {
        assert_eq!(
            parse_selector("my-comp, other-comp"),
            "[['my-comp'], ['other-comp']]"
        );
    }

    #[test]
    fn test_class_selector() {
        assert_eq!(parse_selector(".my-class"), "[['', 8, 'my-class']]");
    }

    #[test]
    fn test_element_with_attribute() {
        assert_eq!(
            parse_selector("button[type=submit]"),
            "[['button', 'type', 'submit']]"
        );
    }

    #[test]
    fn test_not_selector() {
        assert_eq!(
            parse_selector("[ngModel]:not([formControlName]):not([formControl])"),
            "[['', 'ngModel', '', 3, 'formControlName', '', 3, 'formControl', '']]"
        );
    }
}
