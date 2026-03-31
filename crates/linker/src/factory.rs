//! Transform `ɵɵngDeclareFactory` → factory function.
//!
//! Converts a partial factory declaration into a fully compiled factory function
//! that Angular can use at runtime.
//!
//! ## Example
//! ```js
//! // Input:
//! i0.ɵɵngDeclareFactory({ type: MyService, deps: [{ token: Dep }], target: ... })
//! // Output:
//! function MyService_Factory(ɵt) { return new (ɵt || MyService)(i0.ɵɵinject(Dep)); }
//! ```

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::{
    ArrayExpressionElement, Expression, ObjectExpression, ObjectPropertyKind, PropertyKey,
};

use crate::metadata;

/// Transform a `ɵɵngDeclareFactory` call into a factory function.
pub fn transform(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> NgcResult<String> {
    let type_name = metadata::get_identifier_prop(obj, "type")
        .or_else(|| metadata::get_source_text(obj, "type", source).map(|s| s.to_string()))
        .unwrap_or_else(|| "Unknown".to_string());

    let deps = build_deps_args(obj, source, ng_import);

    Ok(format!(
        "function {type_name}_Factory(\u{0275}t) {{ return new (\u{0275}t || {type_name})({deps}); }}"
    ))
}

/// Build the constructor arguments from the `deps` array.
fn build_deps_args(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> String {
    // Find the deps array property
    let deps_array = obj.properties.iter().find_map(|p| {
        if let ObjectPropertyKind::ObjectProperty(prop) = p {
            if matches!(&prop.key, PropertyKey::StaticIdentifier(id) if id.name.as_str() == "deps")
            {
                if let Expression::ArrayExpression(arr) = &prop.value {
                    return Some(arr.as_ref());
                }
            }
        }
        None
    });

    let arr = match deps_array {
        Some(a) => a,
        None => return String::new(),
    };

    let inject_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}inject".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}inject")
    };

    let mut args = Vec::new();
    for element in &arr.elements {
        if let ArrayExpressionElement::ObjectExpression(dep_obj) = element {
            if let Some(arg) = transform_dep(dep_obj, source, ng_import, &inject_fn) {
                args.push(arg);
            }
        }
    }

    args.join(", ")
}

/// Transform a single dependency descriptor object into an inject call.
fn transform_dep(
    dep_obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
    inject_fn: &str,
) -> Option<String> {
    // Check for attribute injection first
    if let Some(attr_name) = metadata::get_string_prop(dep_obj, "attribute") {
        let inject_attr = if ng_import.is_empty() {
            format!("\u{0275}\u{0275}injectAttribute('{attr_name}')")
        } else {
            format!("{ng_import}.\u{0275}\u{0275}injectAttribute('{attr_name}')")
        };
        return Some(inject_attr);
    }

    let token = metadata::get_source_text(dep_obj, "token", source)?;

    // Compute flags from optional/self/skipSelf/host
    let mut flags = 0u32;
    if metadata::get_bool_prop(dep_obj, "optional") == Some(true) {
        flags |= 8;
    }
    if metadata::get_bool_prop(dep_obj, "self") == Some(true) {
        flags |= 2;
    }
    if metadata::get_bool_prop(dep_obj, "skipSelf") == Some(true) {
        flags |= 4;
    }
    if metadata::get_bool_prop(dep_obj, "host") == Some(true) {
        flags |= 1;
    }

    if flags != 0 {
        Some(format!("{inject_fn}({token}, {flags})"))
    } else {
        Some(format!("{inject_fn}({token})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn parse_and_transform(input: &str) -> String {
        let alloc = Allocator::default();
        let code = format!("var x = {input};");
        let parsed = Parser::new(&alloc, &code, SourceType::mjs()).parse();
        let program = parsed.program;

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                return transform(obj, &code, "i0").unwrap();
            }
        }
        panic!("failed to parse");
    }

    #[test]
    fn test_factory_no_deps() {
        let result = parse_and_transform("{ type: MyService, deps: [], target: 2 }");
        assert_eq!(
            result,
            "function MyService_Factory(\u{0275}t) { return new (\u{0275}t || MyService)(); }"
        );
    }

    #[test]
    fn test_factory_with_deps() {
        let result =
            parse_and_transform("{ type: MyService, deps: [{ token: i0.DOCUMENT }], target: 2 }");
        assert!(result.contains("i0.\u{0275}\u{0275}inject(i0.DOCUMENT)"));
    }

    #[test]
    fn test_factory_with_optional_dep() {
        let result = parse_and_transform(
            "{ type: MyService, deps: [{ token: SomeDep, optional: true }], target: 2 }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}inject(SomeDep, 8)"));
    }
}
