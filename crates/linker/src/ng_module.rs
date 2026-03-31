//! Transform `ɵɵngDeclareNgModule` → `ɵɵdefineNgModule`.
//!
//! ## Example
//! ```js
//! // Input:
//! i0.ɵɵngDeclareNgModule({ type: AppModule, declarations: [A], imports: [B], exports: [C] })
//! // Output:
//! i0.ɵɵdefineNgModule({ type: AppModule, declarations: [A], imports: [B], exports: [C] })
//! ```

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::ObjectExpression;

use crate::metadata;

/// Transform a `ɵɵngDeclareNgModule` call into a `ɵɵdefineNgModule` call.
pub fn transform(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> NgcResult<String> {
    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}defineNgModule".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}defineNgModule")
    };

    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");

    let mut props = Vec::new();
    props.push(format!("type: {type_text}"));

    for key in &["declarations", "imports", "exports", "bootstrap", "schemas"] {
        if let Some(value) = metadata::get_source_text(obj, key, source) {
            props.push(format!("{key}: {value}"));
        }
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
    fn test_ng_module_basic() {
        let result = parse_and_transform(
            "{ type: AppModule, declarations: [CompA], imports: [ModB], exports: [CompA] }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}defineNgModule"));
        assert!(result.contains("type: AppModule"));
        assert!(result.contains("declarations: [CompA]"));
        assert!(result.contains("imports: [ModB]"));
        assert!(result.contains("exports: [CompA]"));
    }
}
