//! Transform `ɵɵngDeclarePipe` → `ɵɵdefinePipe`.
//!
//! ## Example
//! ```js
//! // Input:
//! i0.ɵɵngDeclarePipe({ type: AsyncPipe, name: 'async', pure: false, standalone: true })
//! // Output:
//! i0.ɵɵdefinePipe({ name: 'async', type: AsyncPipe, pure: false, standalone: true })
//! ```

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::ObjectExpression;

use crate::metadata;

/// Transform a `ɵɵngDeclarePipe` call into a `ɵɵdefinePipe` call.
pub fn transform(obj: &ObjectExpression<'_>, source: &str, ng_import: &str) -> NgcResult<String> {
    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}definePipe".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}definePipe")
    };

    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");

    let mut props = Vec::new();

    // name comes first in the runtime format
    if let Some(name) = metadata::get_source_text(obj, "name", source) {
        props.push(format!("name: {name}"));
    }

    props.push(format!("type: {type_text}"));

    if let Some(pure) = metadata::get_source_text(obj, "pure", source) {
        props.push(format!("pure: {pure}"));
    }

    if metadata::get_bool_prop(obj, "standalone") == Some(true) {
        props.push("standalone: true".to_string());
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
    fn test_pipe_basic() {
        let result = parse_and_transform(
            "{ type: AsyncPipe, name: 'async', pure: false, standalone: true }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}definePipe"));
        assert!(result.contains("name: 'async'"));
        assert!(result.contains("type: AsyncPipe"));
        assert!(result.contains("pure: false"));
        assert!(result.contains("standalone: true"));
    }
}
