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
use crate::module_registry::{parse_identifier_array, ModuleRegistry};

/// Transform a `ɵɵngDeclareNgModule` call into a `ɵɵdefineNgModule` call.
///
/// As a side effect, records the module's `type` name and its direct `exports`
/// list in `registry`. Transitive flattening happens later at lookup time.
pub fn transform(
    obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
    registry: &ModuleRegistry,
) -> NgcResult<String> {
    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}defineNgModule".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}defineNgModule")
    };

    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");

    let mut props = Vec::new();
    props.push(format!("type: {type_text}"));

    let mut exports_src: Option<&str> = None;
    for key in &["declarations", "imports", "exports", "bootstrap", "schemas"] {
        if let Some(value) = metadata::get_source_text(obj, key, source) {
            props.push(format!("{key}: {value}"));
            if *key == "exports" {
                exports_src = Some(value);
            }
        }
    }

    if let Some(type_name) = metadata::get_identifier_prop(obj, "type") {
        let exports = exports_src
            .and_then(parse_identifier_array)
            .unwrap_or_default();
        registry.register(&type_name, exports);
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

    fn parse_and_transform(input: &str) -> (String, ModuleRegistry) {
        let alloc = Allocator::default();
        let code = format!("var x = {input};");
        let parsed = Parser::new(&alloc, &code, SourceType::mjs()).parse();
        let program = parsed.program;
        let registry = ModuleRegistry::new();

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                let out = transform(obj, &code, "i0", &registry).unwrap();
                return (out, registry);
            }
        }
        panic!("failed to parse");
    }

    #[test]
    fn test_ng_module_basic() {
        let (result, _) = parse_and_transform(
            "{ type: AppModule, declarations: [CompA], imports: [ModB], exports: [CompA] }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}defineNgModule"));
        assert!(result.contains("type: AppModule"));
        assert!(result.contains("declarations: [CompA]"));
        assert!(result.contains("imports: [ModB]"));
        assert!(result.contains("exports: [CompA]"));
    }

    #[test]
    fn test_ng_module_registers_exports() {
        let (_, registry) = parse_and_transform(
            "{ type: ReactiveFormsModule, exports: [InternalShared, FormGroupDirective, FormControlName] }",
        );
        assert!(registry.is_module("ReactiveFormsModule"));
        assert_eq!(
            registry.flatten("ReactiveFormsModule"),
            vec!["InternalShared", "FormGroupDirective", "FormControlName"]
        );
    }

    #[test]
    fn test_ng_module_registers_even_without_exports() {
        let (_, registry) = parse_and_transform("{ type: EmptyModule }");
        assert!(registry.is_module("EmptyModule"));
        assert!(registry.flatten("EmptyModule").is_empty());
    }
}
