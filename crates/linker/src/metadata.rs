//! Helpers for extracting property values from oxc `ObjectExpression` AST nodes.
//!
//! These functions use source-text spans to extract values, avoiding the need
//! to reconstruct code from AST nodes.

use oxc_ast::ast::{Expression, ObjectExpression, ObjectPropertyKind, PropertyKey};
use oxc_span::GetSpan;

/// Extract the source text for a named property's value from an object literal.
///
/// Returns the raw source text of the value expression, suitable for
/// pass-through into generated code.
pub fn get_source_text<'a>(
    obj: &ObjectExpression<'_>,
    key: &str,
    source: &'a str,
) -> Option<&'a str> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            if property_key_matches(&p.key, key) {
                let span = p.value.span();
                return Some(&source[span.start as usize..span.end as usize]);
            }
        }
    }
    None
}

/// Extract a string literal value for a named property.
///
/// Returns the string contents (without quotes) if the property exists
/// and its value is a string literal.
pub fn get_string_prop(obj: &ObjectExpression<'_>, key: &str) -> Option<String> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            if property_key_matches(&p.key, key) {
                match &p.value {
                    Expression::StringLiteral(s) => return Some(s.value.to_string()),
                    Expression::TemplateLiteral(t) if t.expressions.is_empty() => {
                        // Simple template literal with no interpolations (e.g. `hello`)
                        if let Some(quasi) = t.quasis.first() {
                            return Some(quasi.value.raw.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

/// Extract a string-array value for a named property.
///
/// Returns the string contents of each element if the property exists and
/// its value is an array of string literals
/// (e.g. `exportAs: ['ngForm', 'abc']`).  Returns `None` for non-array
/// forms — callers that need to fall back to a string literal should
/// check `get_string_prop` separately.
pub fn get_string_array_prop(obj: &ObjectExpression<'_>, key: &str) -> Option<Vec<String>> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            if property_key_matches(&p.key, key) {
                if let Expression::ArrayExpression(arr) = &p.value {
                    let mut out = Vec::with_capacity(arr.elements.len());
                    for elem in &arr.elements {
                        if let oxc_ast::ast::ArrayExpressionElement::StringLiteral(s) = elem {
                            out.push(s.value.to_string());
                        } else {
                            return None;
                        }
                    }
                    return Some(out);
                }
            }
        }
    }
    None
}

/// Extract a boolean literal value for a named property.
pub fn get_bool_prop(obj: &ObjectExpression<'_>, key: &str) -> Option<bool> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            if property_key_matches(&p.key, key) {
                if let Expression::BooleanLiteral(b) = &p.value {
                    return Some(b.value);
                }
            }
        }
    }
    None
}

/// Extract an identifier name for a named property.
///
/// Returns the identifier text if the property exists and its value is
/// a simple identifier (e.g., `type: MyService` → `"MyService"`).
pub fn get_identifier_prop(obj: &ObjectExpression<'_>, key: &str) -> Option<String> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            if property_key_matches(&p.key, key) {
                if let Expression::Identifier(id) = &p.value {
                    return Some(id.name.to_string());
                }
            }
        }
    }
    None
}

/// Get the `ObjectExpression` value for a named property.
///
/// Returns a reference to the inner object if the property exists and
/// its value is an object literal.
pub fn get_object_prop<'a>(
    obj: &'a ObjectExpression<'_>,
    key: &str,
) -> Option<&'a ObjectExpression<'a>> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            if property_key_matches(&p.key, key) {
                if let Expression::ObjectExpression(inner) = &p.value {
                    return Some(inner);
                }
            }
        }
    }
    None
}

/// Check whether a `PropertyKey` matches a given key name.
fn property_key_matches(pk: &PropertyKey<'_>, key: &str) -> bool {
    match pk {
        PropertyKey::StaticIdentifier(id) => id.name.as_str() == key,
        PropertyKey::StringLiteral(s) => s.value.as_str() == key,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    #[test]
    fn test_get_string_prop() {
        let alloc = Allocator::default();
        let code = "var x = { name: 'hello', count: 42 };";
        let parsed = Parser::new(&alloc, code, SourceType::mjs()).parse();
        let program = parsed.program;

        // Navigate to the object expression
        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                assert_eq!(get_string_prop(obj, "name"), Some("hello".to_string()));
                assert_eq!(get_string_prop(obj, "count"), None); // not a string
                assert_eq!(get_string_prop(obj, "missing"), None);
                return;
            }
        }
        panic!("failed to parse test object");
    }

    #[test]
    fn test_get_source_text() {
        let alloc = Allocator::default();
        let code = "var x = { deps: [a, b, c], name: 'test' };";
        let parsed = Parser::new(&alloc, code, SourceType::mjs()).parse();
        let program = parsed.program;

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                assert_eq!(get_source_text(obj, "deps", code), Some("[a, b, c]"));
                assert_eq!(get_source_text(obj, "name", code), Some("'test'"));
                return;
            }
        }
        panic!("failed to parse test object");
    }

    #[test]
    fn test_get_bool_prop() {
        let alloc = Allocator::default();
        let code = "var x = { standalone: true, pure: false };";
        let parsed = Parser::new(&alloc, code, SourceType::mjs()).parse();
        let program = parsed.program;

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                assert_eq!(get_bool_prop(obj, "standalone"), Some(true));
                assert_eq!(get_bool_prop(obj, "pure"), Some(false));
                assert_eq!(get_bool_prop(obj, "missing"), None);
                return;
            }
        }
        panic!("failed to parse test object");
    }

    #[test]
    fn test_get_identifier_prop() {
        let alloc = Allocator::default();
        let code = "var x = { type: MyService, name: 'test' };";
        let parsed = Parser::new(&alloc, code, SourceType::mjs()).parse();
        let program = parsed.program;

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                assert_eq!(
                    get_identifier_prop(obj, "type"),
                    Some("MyService".to_string())
                );
                assert_eq!(get_identifier_prop(obj, "name"), None); // string, not ident
                return;
            }
        }
        panic!("failed to parse test object");
    }
}
