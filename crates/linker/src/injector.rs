//! Transform `ɵɵngDeclareInjector` → `ɵɵdefineInjector`.
//!
//! ## Example
//! ```js
//! // Input:
//! i0.ɵɵngDeclareInjector({ type: AppModule, providers: [...], imports: [...] })
//! // Output:
//! i0.ɵɵdefineInjector({ providers: [...], imports: [...] })
//! ```

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::ObjectExpression;

use crate::metadata;

/// Transform a `ɵɵngDeclareInjector` call into a `ɵɵdefineInjector` call.
pub fn transform(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> NgcResult<String> {
    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}defineInjector".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}defineInjector")
    };

    let mut props = Vec::new();

    if let Some(providers) = metadata::get_source_text(obj, "providers", source) {
        props.push(format!("providers: {providers}"));
    }

    if let Some(imports) = metadata::get_source_text(obj, "imports", source) {
        props.push(format!("imports: {imports}"));
    }

    if props.is_empty() {
        Ok(format!("{define_fn}({{}})"))
    } else {
        Ok(format!("{define_fn}({{ {} }})", props.join(", ")))
    }
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
    fn test_injector_with_providers_and_imports() {
        let result =
            parse_and_transform("{ type: AppModule, providers: [ServiceA], imports: [ModuleB] }");
        assert!(result.contains("i0.\u{0275}\u{0275}defineInjector"));
        assert!(result.contains("providers: [ServiceA]"));
        assert!(result.contains("imports: [ModuleB]"));
    }

    #[test]
    fn test_injector_empty() {
        let result = parse_and_transform("{ type: AppModule }");
        assert_eq!(result, "i0.\u{0275}\u{0275}defineInjector({})");
    }
}
