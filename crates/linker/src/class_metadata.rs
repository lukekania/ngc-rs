//! Transform `ɵɵngDeclareClassMetadata` → `ɵsetClassMetadata`.
//!
//! ## Example
//! ```js
//! // Input:
//! i0.ɵɵngDeclareClassMetadata({ type: MyService, decorators: [...] })
//! // Output:
//! (function() { i0.ɵsetClassMetadata(MyService, [...], null, null); })()
//! ```

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::ObjectExpression;

use crate::metadata;

/// Transform a `ɵɵngDeclareClassMetadata` call into a `ɵsetClassMetadata` call.
pub fn transform(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> NgcResult<String> {
    let set_fn = if ng_import.is_empty() {
        "\u{0275}setClassMetadata".to_string()
    } else {
        format!("{ng_import}.\u{0275}setClassMetadata")
    };

    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");

    let decorators = metadata::get_source_text(obj, "decorators", source).unwrap_or("[]");

    let ctor_params = metadata::get_source_text(obj, "ctorParameters", source).unwrap_or("null");

    let prop_decorators =
        metadata::get_source_text(obj, "propDecorators", source).unwrap_or("null");

    Ok(format!(
        "(function() {{ {set_fn}({type_text}, {decorators}, {ctor_params}, {prop_decorators}); }})()"
    ))
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
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                return transform(obj, &code, "i0").unwrap();
            }
        }
        panic!("failed to parse");
    }

    #[test]
    fn test_class_metadata_basic() {
        let result = parse_and_transform(
            "{ type: MyService, decorators: [{ type: Injectable, args: [{ providedIn: 'root' }] }] }",
        );
        assert!(result.contains("i0.\u{0275}setClassMetadata"));
        assert!(result.contains("MyService"));
        assert!(result.starts_with("(function()"));
        assert!(result.ends_with("})()"));
    }

    #[test]
    fn test_class_metadata_with_ctor_params() {
        let result = parse_and_transform(
            "{ type: MyService, decorators: [], ctorParameters: () => [{ type: HttpClient }] }",
        );
        assert!(result.contains("() => [{ type: HttpClient }]"));
    }
}
