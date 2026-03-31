//! Transform `ɵɵngDeclareInjectable` → `ɵɵdefineInjectable`.
//!
//! ## Example
//! ```js
//! // Input:
//! i0.ɵɵngDeclareInjectable({ type: MyService, providedIn: 'root' })
//! // Output:
//! i0.ɵɵdefineInjectable({ token: MyService, factory: MyService.ɵfac, providedIn: 'root' })
//! ```

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::ObjectExpression;

use crate::metadata;

/// Transform a `ɵɵngDeclareInjectable` call into a `ɵɵdefineInjectable` call.
pub fn transform(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> NgcResult<String> {
    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");

    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}defineInjectable".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}defineInjectable")
    };

    // Determine the factory expression
    let factory = if let Some(use_factory) = metadata::get_source_text(obj, "useFactory", source) {
        use_factory.to_string()
    } else if let Some(use_class) = metadata::get_source_text(obj, "useClass", source) {
        format!("() => new {use_class}()")
    } else if let Some(use_value) = metadata::get_source_text(obj, "useValue", source) {
        format!("() => {use_value}")
    } else if let Some(use_existing) = metadata::get_source_text(obj, "useExisting", source) {
        let inject_fn = if ng_import.is_empty() {
            "\u{0275}\u{0275}inject".to_string()
        } else {
            format!("{ng_import}.\u{0275}\u{0275}inject")
        };
        format!("function() {{ return {inject_fn}({use_existing}); }}")
    } else {
        format!("{type_text}.\u{0275}fac")
    };

    // Build the output object
    let mut props = Vec::new();
    props.push(format!("token: {type_text}"));
    props.push(format!("factory: {factory}"));

    if let Some(provided_in) = metadata::get_source_text(obj, "providedIn", source) {
        props.push(format!("providedIn: {provided_in}"));
    }

    Ok(format!("{define_fn}({{ {} }})", props.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_ast::ast::Expression;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn parse_and_transform(input: &str) -> String {
        let alloc = Allocator::default();
        let code = format!("var x = {input};");
        let parsed = Parser::new(&alloc, &code, SourceType::mjs()).parse();
        let program = parsed.program;

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(init) = &decl.declarations[0].init {
                if let Expression::ObjectExpression(obj) = init {
                    return transform(obj, &code, "i0").unwrap();
                }
            }
        }
        panic!("failed to parse");
    }

    #[test]
    fn test_basic_injectable() {
        let result = parse_and_transform("{ type: MyService, providedIn: 'root' }");
        assert!(result.contains("i0.\u{0275}\u{0275}defineInjectable"));
        assert!(result.contains("token: MyService"));
        assert!(result.contains("factory: MyService.\u{0275}fac"));
        assert!(result.contains("providedIn: 'root'"));
    }

    #[test]
    fn test_injectable_with_use_factory() {
        let result = parse_and_transform(
            "{ type: MyService, providedIn: 'root', useFactory: createService }",
        );
        assert!(result.contains("factory: createService"));
    }

    #[test]
    fn test_injectable_with_use_existing() {
        let result = parse_and_transform("{ type: MyService, useExisting: OtherService }");
        assert!(result.contains("i0.\u{0275}\u{0275}inject(OtherService)"));
    }
}
