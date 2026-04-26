//! Shared host-binding codegen used by both AOT (`@HostListener` /
//! `@HostBinding` decorator extraction) and the linker (partial-compiled
//! npm packages).
//!
//! Each path collects a list of `(target, expression)` property bindings and
//! `(event, expression)` listeners, then this module dispatches each entry
//! to the right Ivy instruction (`ɵɵproperty`, `ɵɵattribute`, `ɵɵclassProp`,
//! `ɵɵstyleProp`, `ɵɵclassMap`, `ɵɵlistener`) and assembles the
//! `hostBindings` function so AOT and linker output match byte-for-byte.

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// Prefix used for namespaced runtime calls (e.g. `i0.`).
fn ng_prefix(ng_import: &str) -> String {
    if ng_import.is_empty() {
        String::new()
    } else {
        format!("{ng_import}.")
    }
}

/// Dispatch a single host property binding to the correct Ivy instruction.
///
/// Returns `(statement, host_vars_added)`.
///
/// Supported targets:
/// - `style.X`           → `ɵɵstyleProp("X", expr)` (+2 vars)
/// - `style.X.unit`      → `ɵɵstyleProp("X", expr, "unit")` (+2 vars)
/// - `class.X`           → `ɵɵclassProp("X", expr)` (+2 vars)
/// - `class`             → `ɵɵclassMap(expr)` (+2 vars)
/// - `attr.X`            → `ɵɵattribute("X", expr)` (+1 var)
/// - bare property `X`   → `ɵɵproperty("X", expr)` (+1 var)
pub fn dispatch_property_binding(
    target: &str,
    raw_expression: &str,
    ng_import: &str,
) -> (String, u32) {
    let prefix = ng_prefix(ng_import);
    let expr = compile_host_expression(raw_expression);

    if let Some(rest) = target.strip_prefix("style.") {
        // `style.width.px` → propName=width, unit=px
        if let Some(dot) = rest.find('.') {
            let prop = &rest[..dot];
            let unit = &rest[dot + 1..];
            (
                format!("{prefix}\u{0275}\u{0275}styleProp(\"{prop}\", {expr}, \"{unit}\")"),
                2,
            )
        } else {
            (
                format!("{prefix}\u{0275}\u{0275}styleProp(\"{rest}\", {expr})"),
                2,
            )
        }
    } else if let Some(class_name) = target.strip_prefix("class.") {
        (
            format!("{prefix}\u{0275}\u{0275}classProp(\"{class_name}\", {expr})"),
            2,
        )
    } else if target == "class" {
        (format!("{prefix}\u{0275}\u{0275}classMap({expr})"), 2)
    } else if let Some(attr_name) = target.strip_prefix("attr.") {
        (
            format!("{prefix}\u{0275}\u{0275}attribute(\"{attr_name}\", {expr})"),
            1,
        )
    } else {
        (
            format!("{prefix}\u{0275}\u{0275}property(\"{target}\", {expr})"),
            1,
        )
    }
}

/// Dispatch a single host event listener to a `ɵɵlistener` call.
///
/// `raw_expression` is the handler invocation source (e.g. `"onClick($event)"`);
/// it is run through [`compile_host_expression`] so component members are
/// prefixed with `ctx.`.
pub fn dispatch_listener(event: &str, raw_expression: &str, ng_import: &str) -> String {
    let prefix = ng_prefix(ng_import);
    let expr = compile_host_expression(raw_expression);
    format!("{prefix}\u{0275}\u{0275}listener(\"{event}\", function($event) {{ return {expr}; }})")
}

/// Assemble the `hostBindings` function literal from already-dispatched
/// statements. Returns `None` when both lists are empty.
///
/// Listeners are emitted under `if (rf & 1)` (creation) and bindings under
/// `if (rf & 2)` (update) — matching Angular's compiler-cli output.
pub fn build_host_bindings_function(
    binding_stmts: &[String],
    listener_stmts: &[String],
) -> Option<String> {
    if binding_stmts.is_empty() && listener_stmts.is_empty() {
        return None;
    }
    let mut body = String::new();
    if !listener_stmts.is_empty() {
        body.push_str("if (rf & 1) { ");
        for stmt in listener_stmts {
            body.push_str(stmt);
            body.push_str("; ");
        }
        body.push_str("} ");
    }
    if !binding_stmts.is_empty() {
        body.push_str("if (rf & 2) { ");
        for stmt in binding_stmts {
            body.push_str(stmt);
            body.push_str("; ");
        }
        body.push('}');
    }
    Some(format!("function(rf, ctx) {{ {body} }}"))
}

/// Compile a host binding expression by prefixing component property
/// references with `ctx.` and stripping TypeScript-only syntax (the `!`
/// non-null assertion, `$any(...)` template-DSL coercion).
///
/// Examples:
/// - `"checked"` → `"ctx.checked"`
/// - `"!!checked"` → `"!!ctx.checked"`
/// - `"disabled || null"` → `"ctx.disabled || null"`
/// - `"onChange($any($event.target).checked)"` → `"ctx.onChange($event.target.checked)"`
pub fn compile_host_expression(expr: &str) -> String {
    let wrapper = format!("var __expr = {expr};");
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, &wrapper, SourceType::tsx()).parse();

    if !parsed.errors.is_empty() {
        tracing::warn!("failed to parse host expression: {expr}");
        return expr.to_string();
    }

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

    let mut ctx_inserts: Vec<u32> = Vec::new();
    let mut remove_ranges: Vec<(u32, u32)> = Vec::new();

    collect_ctx_rewrites(init_expr, &mut ctx_inserts, &mut remove_ranges, false);

    let expr_offset = "var __expr = ".len() as u32;

    let mut result = expr.to_string();

    ctx_inserts.sort_unstable();
    ctx_inserts.dedup();

    enum Edit {
        Insert(u32),
        Remove(u32, u32),
    }
    let mut edits: Vec<Edit> = Vec::with_capacity(ctx_inserts.len() + remove_ranges.len());
    for off in &ctx_inserts {
        edits.push(Edit::Insert(off - expr_offset));
    }
    for (s, e) in &remove_ranges {
        edits.push(Edit::Remove(s - expr_offset, e - expr_offset));
    }
    edits.sort_by_key(|e| {
        std::cmp::Reverse(match e {
            Edit::Insert(p) => *p,
            Edit::Remove(s, _) => *s,
        })
    });

    for edit in edits {
        match edit {
            Edit::Insert(off) => {
                let off = off as usize;
                if off <= result.len() {
                    result.insert_str(off, "ctx.");
                }
            }
            Edit::Remove(s, e) => {
                let (s, e) = (s as usize, e as usize);
                if s <= result.len() && e <= result.len() {
                    result.replace_range(s..e, "");
                }
            }
        }
    }

    result
}

/// Recursively collect identifier positions that need a `ctx.` prefix and
/// TypeScript non-null assertion / `$any(...)` ranges to remove.
fn collect_ctx_rewrites(
    expr: &oxc_ast::ast::Expression<'_>,
    ctx_inserts: &mut Vec<u32>,
    remove_ranges: &mut Vec<(u32, u32)>,
    is_member_property: bool,
) {
    use oxc_ast::ast::*;

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
                | "ctx"
        )
    }

    match expr {
        Expression::Identifier(id) if !is_member_property && !is_builtin(&id.name) => {
            ctx_inserts.push(id.span.start);
        }
        Expression::CallExpression(call) => {
            if let Expression::Identifier(id) = &call.callee {
                if id.name == "$any" && call.arguments.len() == 1 {
                    if let Some(arg) = call.arguments.first() {
                        if !matches!(arg, Argument::SpreadElement(_)) {
                            let inner = arg.to_expression();
                            remove_ranges.push((call.span.start, inner.span().start));
                            remove_ranges.push((inner.span().end, call.span.end));
                            collect_ctx_rewrites(
                                inner,
                                ctx_inserts,
                                remove_ranges,
                                is_member_property,
                            );
                            return;
                        }
                    }
                }
            }
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
            collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false);
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
            collect_ctx_rewrites(
                &non_null.expression,
                ctx_inserts,
                remove_ranges,
                is_member_property,
            );
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
        | Expression::NullLiteral(_) => {}
        _ => {}
    }
}

/// Transform a `hostDirectives` array source-text into the runtime form.
///
/// The source-level / partial-declaration form uses `'public: private'` colon
/// syntax for `inputs` / `outputs` remapping. Angular's runtime
/// `bindingArrayToMap` reads the array as flat pairs (`bindings[i]`,
/// `bindings[i+1]`), so the colon strings must be split into pair entries
/// before they reach `ɵɵHostDirectivesFeature(...)`.
///
/// Bare class refs (`[Foo]`) and any unrecognised entry shape pass through
/// verbatim. A bare name `'x'` (no colon) becomes the identity pair `'x', 'x'`.
///
/// Returns `None` if the input fails to parse as an array literal — callers
/// should fall back to passing the source through unchanged so codegen still
/// produces something compilable.
pub fn transform_host_directives_array(source_text: &str) -> Option<String> {
    use oxc_ast::ast::{
        ArrayExpressionElement, Expression, ObjectExpression, ObjectPropertyKind, PropertyKey,
        Statement,
    };

    let alloc = Allocator::default();
    let wrapped = format!("var __x = {source_text};");
    let parsed = Parser::new(&alloc, &wrapped, SourceType::mjs()).parse();
    if !parsed.errors.is_empty() {
        return None;
    }

    let arr = match parsed.program.body.first() {
        Some(Statement::VariableDeclaration(decl)) => match decl.declarations.first() {
            Some(d) => match &d.init {
                Some(Expression::ArrayExpression(a)) => a,
                _ => return None,
            },
            None => return None,
        },
        _ => return None,
    };

    fn span_text<'a, T: GetSpan>(node: &T, src: &'a str) -> &'a str {
        let sp = node.span();
        &src[sp.start as usize..sp.end as usize]
    }

    fn flatten_binding_pair_strings(arr: &oxc_ast::ast::ArrayExpression<'_>) -> Vec<String> {
        let mut out = Vec::with_capacity(arr.elements.len() * 2);
        for el in &arr.elements {
            if let ArrayExpressionElement::StringLiteral(s) = el {
                let raw = s.value.as_str();
                if let Some(idx) = raw.find(':') {
                    let pub_name = raw[..idx].trim();
                    let priv_name = raw[idx + 1..].trim();
                    out.push(format!("'{pub_name}'"));
                    out.push(format!("'{priv_name}'"));
                } else {
                    let n = raw.trim();
                    out.push(format!("'{n}'"));
                    out.push(format!("'{n}'"));
                }
            }
        }
        out
    }

    fn rewrite_object(obj: &ObjectExpression<'_>, src: &str) -> String {
        let mut props = Vec::with_capacity(obj.properties.len());
        for prop in &obj.properties {
            let ObjectPropertyKind::ObjectProperty(p) = prop else {
                continue;
            };
            let key = match &p.key {
                PropertyKey::StaticIdentifier(id) => id.name.as_str(),
                PropertyKey::StringLiteral(s) => s.value.as_str(),
                _ => continue,
            };
            match key {
                "inputs" | "outputs" => {
                    if let Expression::ArrayExpression(arr) = &p.value {
                        let pairs = flatten_binding_pair_strings(arr);
                        props.push(format!("{key}: [{}]", pairs.join(", ")));
                    } else {
                        props.push(format!("{key}: {}", span_text(&p.value, src)));
                    }
                }
                _ => {
                    props.push(format!("{key}: {}", span_text(&p.value, src)));
                }
            }
        }
        format!("{{ {} }}", props.join(", "))
    }

    let mut entries = Vec::with_capacity(arr.elements.len());
    for el in &arr.elements {
        match el {
            ArrayExpressionElement::ObjectExpression(obj) => {
                entries.push(rewrite_object(obj, &wrapped));
            }
            other => {
                entries.push(span_text(other, &wrapped).to_string());
            }
        }
    }
    Some(format!("[{}]", entries.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_dispatch_bare() {
        let (stmt, vars) = dispatch_property_binding("disabled", "isDisabled", "i0");
        assert_eq!(
            stmt,
            "i0.\u{0275}\u{0275}property(\"disabled\", ctx.isDisabled)"
        );
        assert_eq!(vars, 1);
    }

    #[test]
    fn property_dispatch_attr() {
        let (stmt, vars) = dispatch_property_binding("attr.aria-label", "label", "i0");
        assert_eq!(
            stmt,
            "i0.\u{0275}\u{0275}attribute(\"aria-label\", ctx.label)"
        );
        assert_eq!(vars, 1);
    }

    #[test]
    fn property_dispatch_class_prop() {
        let (stmt, vars) = dispatch_property_binding("class.active", "isActive", "i0");
        assert_eq!(
            stmt,
            "i0.\u{0275}\u{0275}classProp(\"active\", ctx.isActive)"
        );
        assert_eq!(vars, 2);
    }

    #[test]
    fn property_dispatch_class_map() {
        let (stmt, vars) = dispatch_property_binding("class", "extraClasses", "i0");
        assert_eq!(stmt, "i0.\u{0275}\u{0275}classMap(ctx.extraClasses)");
        assert_eq!(vars, 2);
    }

    #[test]
    fn property_dispatch_style_simple() {
        let (stmt, vars) = dispatch_property_binding("style.color", "color", "i0");
        assert_eq!(stmt, "i0.\u{0275}\u{0275}styleProp(\"color\", ctx.color)");
        assert_eq!(vars, 2);
    }

    #[test]
    fn property_dispatch_style_with_unit() {
        let (stmt, vars) = dispatch_property_binding("style.width.px", "width", "i0");
        assert_eq!(
            stmt,
            "i0.\u{0275}\u{0275}styleProp(\"width\", ctx.width, \"px\")"
        );
        assert_eq!(vars, 2);
    }

    #[test]
    fn listener_dispatch_basic() {
        let stmt = dispatch_listener("click", "onClick($event)", "i0");
        assert_eq!(
            stmt,
            "i0.\u{0275}\u{0275}listener(\"click\", function($event) { return ctx.onClick($event); })"
        );
    }

    #[test]
    fn listener_dispatch_global_target() {
        let stmt = dispatch_listener("window:resize", "onResize()", "i0");
        assert_eq!(
            stmt,
            "i0.\u{0275}\u{0275}listener(\"window:resize\", function($event) { return ctx.onResize(); })"
        );
    }

    #[test]
    fn build_function_listener_only() {
        let listener = vec![dispatch_listener("click", "onClick()", "i0")];
        let out = build_host_bindings_function(&[], &listener).unwrap();
        assert!(out.starts_with("function(rf, ctx) { if (rf & 1) {"));
        assert!(!out.contains("if (rf & 2)"));
    }

    #[test]
    fn build_function_binding_only() {
        let (stmt, _) = dispatch_property_binding("attr.title", "title", "i0");
        let out = build_host_bindings_function(&[stmt], &[]).unwrap();
        assert!(!out.contains("if (rf & 1)"));
        assert!(out.contains("if (rf & 2) {"));
    }

    #[test]
    fn build_function_empty_returns_none() {
        assert!(build_host_bindings_function(&[], &[]).is_none());
    }

    #[test]
    fn ng_import_empty_omits_prefix() {
        let (stmt, _) = dispatch_property_binding("disabled", "isDisabled", "");
        assert_eq!(
            stmt,
            "\u{0275}\u{0275}property(\"disabled\", ctx.isDisabled)"
        );
    }

    #[test]
    fn host_expression_simple_identifier() {
        assert_eq!(compile_host_expression("checked"), "ctx.checked");
    }

    #[test]
    fn host_expression_negation() {
        assert_eq!(compile_host_expression("!!checked"), "!!ctx.checked");
    }

    #[test]
    fn host_expression_logical() {
        assert_eq!(
            compile_host_expression("disabled || null"),
            "ctx.disabled || null"
        );
    }

    #[test]
    fn host_expression_method_call() {
        assert_eq!(
            compile_host_expression("toastClasses()"),
            "ctx.toastClasses()"
        );
    }

    #[test]
    fn host_expression_member_chain() {
        assert_eq!(
            compile_host_expression("_rangeInput.rangePicker ? \"dialog\" : null"),
            "ctx._rangeInput.rangePicker ? \"dialog\" : null"
        );
    }

    #[test]
    fn host_expression_ts_non_null() {
        assert_eq!(
            compile_host_expression("_getMinDate()!"),
            "ctx._getMinDate()"
        );
    }

    #[test]
    fn host_expression_any_in_listener() {
        assert_eq!(
            compile_host_expression("onChange($any($event.target).checked)"),
            "ctx.onChange($event.target.checked)"
        );
    }

    #[test]
    fn host_expression_any_over_ctx() {
        assert_eq!(
            compile_host_expression("$any(ctx).foo.bar()"),
            "ctx.foo.bar()"
        );
    }

    #[test]
    fn host_expression_any_bare_identifier() {
        assert_eq!(compile_host_expression("$any(value)"), "ctx.value");
    }

    #[test]
    fn transform_host_directives_bare_only() {
        // Bare class refs pass through verbatim — `ɵɵHostDirectivesFeature`
        // accepts them directly and the runtime treats them as identity-mapped
        // composed directives with no input/output remapping.
        let out = transform_host_directives_array("[Foo, Bar]").unwrap();
        assert_eq!(out, "[Foo, Bar]");
    }

    #[test]
    fn transform_host_directives_bare_input_to_identity_pair() {
        // A bare `'name'` input/output entry must expand to the identity pair
        // `'name', 'name'` so the runtime's `bindingArrayToMap` reads it as a
        // pair (key = name, value = name).
        let out = transform_host_directives_array(
            "[{ directive: Foo, inputs: ['x'], outputs: ['evt'] }]",
        )
        .unwrap();
        assert!(
            out.contains("inputs: ['x', 'x']"),
            "bare input must expand to identity pair: {out}"
        );
        assert!(
            out.contains("outputs: ['evt', 'evt']"),
            "bare output must expand to identity pair: {out}"
        );
    }

    #[test]
    fn transform_host_directives_colon_to_flat_pair() {
        // The decorator/partial-form `'public: private'` colon syntax must be
        // split into a flat pair (`'public', 'private'`). Trim spaces around
        // the colon so `'a: b'` and `'a:b'` are equivalent.
        let out = transform_host_directives_array(
            "[{ directive: Foo, inputs: ['a: b', 'c:d'], outputs: ['evt: hostEvt'] }]",
        )
        .unwrap();
        assert!(
            out.contains("inputs: ['a', 'b', 'c', 'd']"),
            "colon-syntax inputs must be split into flat pairs: {out}"
        );
        assert!(
            out.contains("outputs: ['evt', 'hostEvt']"),
            "colon-syntax outputs must be split into flat pairs: {out}"
        );
    }

    #[test]
    fn transform_host_directives_mixed_entries() {
        // A mix of bare class ref and remapped object form must round-trip
        // correctly — bare ref untouched, object's inputs flattened.
        let out =
            transform_host_directives_array("[Foo, { directive: Bar, inputs: ['propA: aliasA'] }]")
                .unwrap();
        assert!(out.starts_with("[Foo,"), "bare ref must stay first: {out}");
        assert!(
            out.contains("inputs: ['propA', 'aliasA']"),
            "object's inputs must be flattened: {out}"
        );
    }

    #[test]
    fn host_expression_complex_ternary() {
        let result = compile_host_expression(
            "_getMinDate() ? _dateAdapter.toIso8601(_getMinDate()!) : null",
        );
        assert!(result.contains("ctx._getMinDate()"));
        assert!(result.contains("ctx._dateAdapter.toIso8601"));
        assert!(!result.contains("!)"));
    }
}
