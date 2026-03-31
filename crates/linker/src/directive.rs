//! Transform `ɵɵngDeclareDirective` → `ɵɵdefineDirective`.
//!
//! Handles selector parsing, input/output mapping, host binding generation,
//! and feature flags.

use std::path::Path;

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::{Expression, ObjectExpression, ObjectPropertyKind, PropertyKey};
use oxc_span::GetSpan;

use crate::metadata;
use crate::selector;

/// Transform a `ɵɵngDeclareDirective` call into a `ɵɵdefineDirective` call.
pub fn transform(
    obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
    _file_path: &Path,
) -> NgcResult<String> {
    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}defineDirective".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}defineDirective")
    };

    build_define_call(&define_fn, obj, source, ng_import)
}

/// Build the `ɵɵdefineDirective` (or `ɵɵdefineComponent`) call body.
///
/// This is shared between directive and component transformations.
pub fn build_define_call(
    define_fn: &str,
    obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
) -> NgcResult<String> {
    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");

    let mut props = Vec::new();
    props.push(format!("type: {type_text}"));

    // Parse selector into Angular array format
    if let Some(sel) = metadata::get_string_prop(obj, "selector") {
        props.push(format!("selectors: {}", selector::parse_selector(&sel)));
    }

    // Inputs
    if let Some(inputs) = build_inputs(obj, source) {
        props.push(format!("inputs: {inputs}"));
    }

    // Outputs
    if let Some(outputs) = build_outputs(obj, source) {
        props.push(format!("outputs: {outputs}"));
    }

    // Host bindings
    if let Some(host_obj) = metadata::get_object_prop(obj, "host") {
        let (host_attrs, host_bindings, host_vars) =
            build_host_bindings(host_obj, source, ng_import);
        if let Some(attrs) = host_attrs {
            props.push(format!("hostAttrs: {attrs}"));
        }
        if host_vars > 0 {
            props.push(format!("hostVars: {host_vars}"));
        }
        if let Some(bindings) = host_bindings {
            props.push(format!("hostBindings: {bindings}"));
        }
    }

    // Export as
    if let Some(export_as) = metadata::get_string_prop(obj, "exportAs") {
        let parts: Vec<&str> = export_as.split(',').map(|s| s.trim()).collect();
        let arr = parts
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        props.push(format!("exportAs: [{arr}]"));
    }

    // Standalone
    if metadata::get_bool_prop(obj, "isStandalone") == Some(true)
        || metadata::get_bool_prop(obj, "standalone") == Some(true)
    {
        props.push("standalone: true".to_string());
    }

    // Features — only emit features that exist in the Angular runtime.
    // Note: ɵɵStandaloneFeature was removed in Angular 19+; standalone is now
    // handled via the `standalone: true` property on the definition (emitted above).
    let mut features = Vec::new();
    if metadata::get_bool_prop(obj, "usesInheritance") == Some(true) {
        let feat = if ng_import.is_empty() {
            "\u{0275}\u{0275}InheritDefinitionFeature".to_string()
        } else {
            format!("{ng_import}.\u{0275}\u{0275}InheritDefinitionFeature")
        };
        features.push(feat);
    }
    if metadata::get_bool_prop(obj, "usesOnChanges") == Some(true) {
        let feat = if ng_import.is_empty() {
            "\u{0275}\u{0275}NgOnChangesFeature".to_string()
        } else {
            format!("{ng_import}.\u{0275}\u{0275}NgOnChangesFeature")
        };
        features.push(feat);
    }
    if !features.is_empty() {
        props.push(format!("features: [{}]", features.join(", ")));
    }

    // Providers
    if let Some(providers) = metadata::get_source_text(obj, "providers", source) {
        props.push(format!("providers: {providers}"));
    }

    // Queries: contentQueries and viewQuery require compilation from declare format
    // (array of descriptors) to runtime format (functions with ɵɵcontentQuery/ɵɵviewQuery calls).
    // Skipped for now — the raw array format crashes the Angular runtime.
    // TODO: compile query descriptors to query functions

    Ok(format!("{define_fn}({{ {} }})", props.join(", ")))
}

/// Build the `inputs` property from the declare format to runtime format.
///
/// Declare format: `{ propName: { alias: 'aliasName', required: true } }` or `{ propName: 'aliasName' }`
/// Runtime format: `{ propName: [flags, 'publicName', 'propName'] }` or `{ propName: 'propName' }`
fn build_inputs(obj: &ObjectExpression<'_>, source: &str) -> Option<String> {
    let inputs_obj = metadata::get_object_prop(obj, "inputs")?;

    let mut entries = Vec::new();
    for prop in &inputs_obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            let key = prop_key_text(&p.key, source);

            match &p.value {
                Expression::StringLiteral(s) => {
                    // Simple alias: { propName: 'aliasName' }
                    let alias = s.value.as_str();
                    if alias == key {
                        entries.push(format!("{key}: '{key}'"));
                    } else {
                        entries.push(format!("{key}: [0, '{alias}', '{key}']"));
                    }
                }
                Expression::ObjectExpression(input_obj) => {
                    // Complex input: { alias: '...', required: true, transform: ... }
                    let alias = metadata::get_string_prop(input_obj, "alias")
                        .unwrap_or_else(|| key.clone());
                    let required = metadata::get_bool_prop(input_obj, "required") == Some(true);
                    let has_transform =
                        metadata::get_source_text(input_obj, "isSignal", source).is_some();

                    let flags = if required { 1 } else { 0 };

                    if has_transform || required || alias != key {
                        entries.push(format!("{key}: [{flags}, '{alias}', '{key}']"));
                    } else {
                        entries.push(format!("{key}: '{key}'"));
                    }
                }
                _ => {
                    // Pass through as source text
                    let val = &source[p.value.span().start as usize..p.value.span().end as usize];
                    entries.push(format!("{key}: {val}"));
                }
            }
        }
    }

    if entries.is_empty() {
        None
    } else {
        Some(format!("{{ {} }}", entries.join(", ")))
    }
}

/// Build the `outputs` property from declare format to runtime format.
fn build_outputs(obj: &ObjectExpression<'_>, source: &str) -> Option<String> {
    let outputs_obj = metadata::get_object_prop(obj, "outputs")?;

    let mut entries = Vec::new();
    for prop in &outputs_obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            let key = prop_key_text(&p.key, source);
            if let Expression::StringLiteral(s) = &p.value {
                entries.push(format!("{key}: '{}'", s.value));
            } else {
                let val = &source[p.value.span().start as usize..p.value.span().end as usize];
                entries.push(format!("{key}: {val}"));
            }
        }
    }

    if entries.is_empty() {
        None
    } else {
        Some(format!("{{ {} }}", entries.join(", ")))
    }
}

/// Build host attributes and host bindings function from the `host` object.
///
/// Returns `(hostAttrs, hostBindings, hostVars)`.
pub fn build_host_bindings(
    host_obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
) -> (Option<String>, Option<String>, u32) {
    let mut attrs = Vec::new();
    let mut binding_stmts = Vec::new();
    let mut listener_stmts = Vec::new();
    let mut host_vars = 0u32;

    // Static attributes
    if let Some(attributes_obj) = metadata::get_object_prop(host_obj, "attributes") {
        for prop in &attributes_obj.properties {
            if let ObjectPropertyKind::ObjectProperty(p) = prop {
                let key = prop_key_text(&p.key, source);
                if let Expression::StringLiteral(s) = &p.value {
                    attrs.push(format!("'{key}', '{}'", s.value));
                }
            }
        }
    }

    // Class attribute
    if let Some(class_attr) = metadata::get_string_prop(host_obj, "classAttribute") {
        // 1 = AttributeMarker.Classes
        attrs.push("1".to_string());
        for class in class_attr.split_whitespace() {
            attrs.push(format!("'{class}'"));
        }
    }

    // Style attribute
    if let Some(style_attr) = metadata::get_string_prop(host_obj, "styleAttribute") {
        // 2 = AttributeMarker.Styles
        attrs.push("2".to_string());
        attrs.push(format!("'{style_attr}'"));
    }

    // Property bindings
    if let Some(properties_obj) = metadata::get_object_prop(host_obj, "properties") {
        for prop in &properties_obj.properties {
            if let ObjectPropertyKind::ObjectProperty(p) = prop {
                let key = prop_key_text(&p.key, source);
                if let Expression::StringLiteral(s) = &p.value {
                    let prop_fn = if ng_import.is_empty() {
                        "\u{0275}\u{0275}hostProperty".to_string()
                    } else {
                        format!("{ng_import}.\u{0275}\u{0275}hostProperty")
                    };
                    let expr = compile_host_expression(&s.value);
                    binding_stmts.push(format!("{prop_fn}(\"{key}\", {expr})"));
                    host_vars += 1;
                }
            }
        }
    }

    // Listeners
    if let Some(listeners_obj) = metadata::get_object_prop(host_obj, "listeners") {
        for prop in &listeners_obj.properties {
            if let ObjectPropertyKind::ObjectProperty(p) = prop {
                let key = prop_key_text(&p.key, source);
                if let Expression::StringLiteral(s) = &p.value {
                    let listener_fn = if ng_import.is_empty() {
                        "\u{0275}\u{0275}listener".to_string()
                    } else {
                        format!("{ng_import}.\u{0275}\u{0275}listener")
                    };
                    let expr = compile_host_expression(&s.value);
                    listener_stmts.push(format!(
                        "{listener_fn}(\"{key}\", function($event) {{ return {expr}; }})"
                    ));
                }
            }
        }
    }

    let host_attrs = if attrs.is_empty() {
        None
    } else {
        Some(format!("[{}]", attrs.join(", ")))
    };

    let host_bindings = if binding_stmts.is_empty() && listener_stmts.is_empty() {
        None
    } else {
        let mut body = String::new();
        if !listener_stmts.is_empty() {
            body.push_str("if (rf & 1) { ");
            for stmt in &listener_stmts {
                body.push_str(stmt);
                body.push_str("; ");
            }
            body.push_str("} ");
        }
        if !binding_stmts.is_empty() {
            body.push_str("if (rf & 2) { ");
            for stmt in &binding_stmts {
                body.push_str(stmt);
                body.push_str("; ");
            }
            body.push('}');
        }
        Some(format!("function(rf, ctx) {{ {body} }}"))
    };

    (host_attrs, host_bindings, host_vars)
}

/// Extract the text of a property key.
fn prop_key_text(key: &PropertyKey<'_>, source: &str) -> String {
    match key {
        PropertyKey::StaticIdentifier(id) => id.name.to_string(),
        PropertyKey::StringLiteral(s) => s.value.to_string(),
        _ => {
            let span = key.span();
            source[span.start as usize..span.end as usize].to_string()
        }
    }
}

/// Compile a host binding expression by prefixing component property references with `ctx.`
/// and stripping TypeScript-specific syntax (like `!` non-null assertion).
///
/// Parses the expression as TypeScript, walks the AST to find standalone identifiers
/// (not member expression properties or built-in values), and prefixes them with `ctx.`.
///
/// Examples:
/// - `"checked"` → `ctx.checked`
/// - `"!!checked"` → `!!ctx.checked`
/// - `"disabled || null"` → `ctx.disabled || null`
/// - `"_getMinDate() ? _dateAdapter.toIso8601(_getMinDate()!) : null"`
///   → `ctx._getMinDate() ? ctx._dateAdapter.toIso8601(ctx._getMinDate()) : null`
fn compile_host_expression(expr: &str) -> String {
    let wrapper = format!("var __expr = {expr};");
    let alloc = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&alloc, &wrapper, oxc_span::SourceType::tsx()).parse();

    if !parsed.errors.is_empty() {
        // If parsing fails, return the expression as-is (better than crashing)
        tracing::warn!("failed to parse host expression: {expr}");
        return expr.to_string();
    }

    // Extract the expression from `var __expr = <expr>;`
    let init_expr = match &parsed.program.body.first() {
        Some(oxc_ast::ast::Statement::VariableDeclaration(decl)) => {
            decl.declarations.first().and_then(|d| d.init.as_ref())
        }
        _ => None,
    };

    let init_expr = match init_expr {
        Some(e) => e,
        None => return expr.to_string(),
    };

    // Collect byte offsets of identifiers that need `ctx.` prefix
    // and byte ranges of `!` non-null assertions to remove
    let mut ctx_inserts: Vec<u32> = Vec::new();
    let mut remove_ranges: Vec<(u32, u32)> = Vec::new();

    collect_ctx_rewrites(init_expr, &mut ctx_inserts, &mut remove_ranges, false);

    // Apply modifications to the original expression string
    // First, map wrapper offsets back to expression offsets
    let expr_offset = "var __expr = ".len() as u32;

    let mut result = expr.to_string();

    // Sort insertions in reverse order to preserve offsets
    ctx_inserts.sort_unstable();
    ctx_inserts.dedup();

    // Apply removals first (sorted reverse)
    let mut sorted_removes: Vec<(u32, u32)> = remove_ranges
        .iter()
        .map(|(s, e)| (s - expr_offset, e - expr_offset))
        .collect();
    sorted_removes.sort_by(|a, b| b.0.cmp(&a.0));
    for (s, e) in &sorted_removes {
        let s = *s as usize;
        let e = *e as usize;
        if s <= result.len() && e <= result.len() {
            result.replace_range(s..e, "");
        }
    }

    // Apply ctx. insertions (sorted reverse)
    let mut sorted_inserts: Vec<u32> = ctx_inserts.iter().map(|off| off - expr_offset).collect();
    sorted_inserts.sort_unstable();
    sorted_inserts.reverse();
    for off in &sorted_inserts {
        let off = *off as usize;
        if off <= result.len() {
            result.insert_str(off, "ctx.");
        }
    }

    result
}

/// Recursively collect identifier positions that need `ctx.` prefix and
/// TypeScript non-null assertion `!` positions to remove.
fn collect_ctx_rewrites(
    expr: &Expression<'_>,
    ctx_inserts: &mut Vec<u32>,
    remove_ranges: &mut Vec<(u32, u32)>,
    is_member_property: bool,
) {
    use oxc_ast::ast::*;
    use oxc_span::GetSpan;

    /// Set of identifiers that should NOT get `ctx.` prefix.
    fn is_builtin(name: &str) -> bool {
        matches!(
            name,
            "null"
                | "undefined"
                | "true"
                | "false"
                | "NaN"
                | "Infinity"
                | "this"
                | "Math"
                | "Date"
                | "JSON"
                | "console"
                | "window"
                | "document"
                | "Array"
                | "Object"
                | "String"
                | "Number"
                | "Boolean"
                | "Error"
                | "RegExp"
                | "Symbol"
                | "Promise"
                | "Map"
                | "Set"
                | "$event"
        )
    }

    match expr {
        Expression::Identifier(id) => {
            if !is_member_property && !is_builtin(&id.name) {
                ctx_inserts.push(id.span.start);
            }
        }
        Expression::CallExpression(call) => {
            collect_ctx_rewrites(&call.callee, ctx_inserts, remove_ranges, false);
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    collect_ctx_rewrites(&spread.argument, ctx_inserts, remove_ranges, false);
                } else {
                    collect_ctx_rewrites(arg.to_expression(), ctx_inserts, remove_ranges, false);
                }
            }
        }
        Expression::StaticMemberExpression(member) => {
            // Object gets ctx. prefix, property does not
            collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false);
            // property is just an IdentifierName, no rewrite needed
        }
        Expression::ComputedMemberExpression(member) => {
            collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false);
            collect_ctx_rewrites(&member.expression, ctx_inserts, remove_ranges, false);
        }
        Expression::UnaryExpression(unary) => {
            collect_ctx_rewrites(&unary.argument, ctx_inserts, remove_ranges, false);
        }
        Expression::BinaryExpression(binary) => {
            collect_ctx_rewrites(&binary.left, ctx_inserts, remove_ranges, false);
            collect_ctx_rewrites(&binary.right, ctx_inserts, remove_ranges, false);
        }
        Expression::LogicalExpression(logical) => {
            collect_ctx_rewrites(&logical.left, ctx_inserts, remove_ranges, false);
            collect_ctx_rewrites(&logical.right, ctx_inserts, remove_ranges, false);
        }
        Expression::ConditionalExpression(cond) => {
            collect_ctx_rewrites(&cond.test, ctx_inserts, remove_ranges, false);
            collect_ctx_rewrites(&cond.consequent, ctx_inserts, remove_ranges, false);
            collect_ctx_rewrites(&cond.alternate, ctx_inserts, remove_ranges, false);
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_ctx_rewrites(&paren.expression, ctx_inserts, remove_ranges, false);
        }
        Expression::TSNonNullExpression(non_null) => {
            // Process the inner expression, then mark the `!` for removal
            collect_ctx_rewrites(
                &non_null.expression,
                ctx_inserts,
                remove_ranges,
                is_member_property,
            );
            // The `!` is at the end of the expression span, just before the closing
            let inner_end = non_null.expression.span().end;
            let outer_end = non_null.span.end;
            if outer_end > inner_end {
                remove_ranges.push((inner_end, outer_end));
            }
        }
        Expression::TemplateLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_) => {
            // Literals don't need rewriting
        }
        _ => {
            // For other expression types, leave as-is
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;
    use std::path::PathBuf;

    fn parse_and_transform(input: &str) -> String {
        let alloc = Allocator::default();
        let code = format!("var x = {input};");
        let parsed = Parser::new(&alloc, &code, SourceType::mjs()).parse();
        let program = parsed.program;

        if let oxc_ast::ast::Statement::VariableDeclaration(decl) = &program.body[0] {
            if let Some(Expression::ObjectExpression(obj)) = &decl.declarations[0].init {
                return transform(obj, &code, "i0", &PathBuf::from("test.mjs")).unwrap();
            }
        }
        panic!("failed to parse");
    }

    #[test]
    fn test_directive_basic() {
        let result =
            parse_and_transform("{ type: MyDir, selector: '[myDir]', isStandalone: true }");
        assert!(result.contains("i0.\u{0275}\u{0275}defineDirective"));
        assert!(result.contains("type: MyDir"));
        assert!(result.contains("selectors: [['', 'myDir', '']]"));
        assert!(result.contains("standalone: true"));
    }

    #[test]
    fn test_directive_with_inputs_outputs() {
        let result = parse_and_transform(
            "{ type: MyDir, selector: '[myDir]', inputs: { value: 'value' }, outputs: { changed: 'changed' } }",
        );
        assert!(result.contains("inputs:"));
        assert!(result.contains("outputs:"));
    }

    #[test]
    fn test_compile_host_expression_simple() {
        assert_eq!(compile_host_expression("checked"), "ctx.checked");
    }

    #[test]
    fn test_compile_host_expression_negation() {
        assert_eq!(compile_host_expression("!!checked"), "!!ctx.checked");
    }

    #[test]
    fn test_compile_host_expression_logical() {
        assert_eq!(
            compile_host_expression("disabled || null"),
            "ctx.disabled || null"
        );
    }

    #[test]
    fn test_compile_host_expression_method_call() {
        assert_eq!(
            compile_host_expression("toastClasses()"),
            "ctx.toastClasses()"
        );
    }

    #[test]
    fn test_compile_host_expression_member_chain() {
        assert_eq!(
            compile_host_expression("_rangeInput.rangePicker ? \"dialog\" : null"),
            "ctx._rangeInput.rangePicker ? \"dialog\" : null"
        );
    }

    #[test]
    fn test_compile_host_expression_ts_non_null() {
        assert_eq!(
            compile_host_expression("_getMinDate()!"),
            "ctx._getMinDate()"
        );
    }

    #[test]
    fn test_compile_host_expression_complex_ternary() {
        let result = compile_host_expression(
            "_getMinDate() ? _dateAdapter.toIso8601(_getMinDate()!) : null",
        );
        assert!(result.contains("ctx._getMinDate()"));
        assert!(result.contains("ctx._dateAdapter.toIso8601"));
        assert!(!result.contains("!)")); // non-null stripped
    }
}
