//! Transform `ɵɵngDeclareDirective` → `ɵɵdefineDirective`.
//!
//! Handles selector parsing, input/output mapping, host binding generation,
//! and feature flags.

use std::path::Path;

use ngc_diagnostics::NgcResult;
use ngc_template_compiler::host_codegen;
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

    // Export as — ɵɵngDeclareDirective emits `exportAs` as a string array
    // (e.g. `exportAs: ["ngForm"]`) but legacy input may also be a single
    // comma-separated string.  Accept both forms.
    if let Some(export_as_names) = metadata::get_string_array_prop(obj, "exportAs") {
        let arr = export_as_names
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        props.push(format!("exportAs: [{arr}]"));
    } else if let Some(export_as) = metadata::get_string_prop(obj, "exportAs") {
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
    //
    // `ɵɵProvidersFeature(providers[, viewProviders])` is what actually wires
    // directive `providers` into the node injector at instantiation time.
    // Without it, `providers` on the directive def is dead weight — directly
    // visible as `NG0201 No provider for NgControl` when e.g. `FormControlName`
    // declares `{provide: NgControl, useExisting: FormControlName}` but
    // `NgControlStatus` can't resolve it.
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

    // Providers — emit BOTH the `providers` property on the def (for Angular's
    // own introspection) AND a `ɵɵProvidersFeature(providers[, viewProviders])`
    // call in `features` (which is what actually registers them at runtime).
    let providers_src = metadata::get_source_text(obj, "providers", source);
    let view_providers_src = metadata::get_source_text(obj, "viewProviders", source);
    if let Some(providers) = providers_src {
        props.push(format!("providers: {providers}"));
        let providers_feature = if ng_import.is_empty() {
            "\u{0275}\u{0275}ProvidersFeature".to_string()
        } else {
            format!("{ng_import}.\u{0275}\u{0275}ProvidersFeature")
        };
        let call = if let Some(vp) = view_providers_src {
            format!("{providers_feature}({providers}, {vp})")
        } else {
            format!("{providers_feature}({providers})")
        };
        features.push(call);
    }

    // hostDirectives composition (Angular 15+). The partial form stores an
    // array of `{ directive, inputs?, outputs? }` objects; the runtime needs
    // those wrapped in a `ɵɵHostDirectivesFeature(...)` feature call so the
    // composed directives are instantiated on the host element and their
    // input/output remappings are wired up.
    if let Some(host_directives) = build_host_directives_feature(obj, source, ng_import) {
        features.push(host_directives);
    }

    if !features.is_empty() {
        props.push(format!("features: [{}]", features.join(", ")));
    }

    // Content queries: compile from declare format (array of descriptors) to runtime
    // format (contentQueries function with ɵɵcontentQuery/ɵɵloadQuery/ɵɵqueryRefresh calls).
    if let Some(queries) = metadata::get_source_text(obj, "queries", source) {
        if let Some(content_queries_fn) = build_content_queries(queries, ng_import, source, obj) {
            props.push(format!("contentQueries: {content_queries_fn}"));
        }
    }

    // View queries: compile from declare format to runtime format.
    if let Some(view_queries) = metadata::get_source_text(obj, "viewQueries", source) {
        if let Some(view_query_fn) = build_view_queries(view_queries, ng_import, source, obj) {
            props.push(format!("viewQuery: {view_query_fn}"));
        }
    }

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
                Expression::ArrayExpression(arr) => {
                    // Declare format uses a 2-element array `[publicName, declaredName]`.
                    // The Angular 21 runtime format is `[flags, publicName, declaredName, transform?]`
                    // — a leading numeric `flags` value is required or the array
                    // positions shift (publicName ends up where flags should be),
                    // breaking input binding silently (e.g. `[formGroup]` never
                    // propagates to `FormGroupDirective.form`).
                    let elements: Vec<&str> = arr
                        .elements
                        .iter()
                        .map(|el| {
                            let sp = el.span();
                            &source[sp.start as usize..sp.end as usize]
                        })
                        .collect();
                    // If the first element is already a number, assume it's runtime-format.
                    let first_is_number = elements.first().is_some_and(|s| {
                        s.trim().chars().next().is_some_and(|c| c.is_ascii_digit())
                    });
                    let runtime = if first_is_number {
                        format!("[{}]", elements.join(", "))
                    } else {
                        // Prepend `0` flags.
                        format!("[0, {}]", elements.join(", "))
                    };
                    entries.push(format!("{key}: {runtime}"));
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

    // Property bindings — dispatch to the correct Ivy instruction based on property name
    if let Some(properties_obj) = metadata::get_object_prop(host_obj, "properties") {
        for prop in &properties_obj.properties {
            if let ObjectPropertyKind::ObjectProperty(p) = prop {
                let key = prop_key_text(&p.key, source);
                if let Expression::StringLiteral(s) = &p.value {
                    let (stmt, vars) =
                        host_codegen::dispatch_property_binding(&key, &s.value, ng_import);
                    binding_stmts.push(stmt);
                    host_vars += vars;
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
                    listener_stmts.push(host_codegen::dispatch_listener(&key, &s.value, ng_import));
                }
            }
        }
    }

    let host_attrs = if attrs.is_empty() {
        None
    } else {
        Some(format!("[{}]", attrs.join(", ")))
    };

    let host_bindings = host_codegen::build_host_bindings_function(&binding_stmts, &listener_stmts);

    (host_attrs, host_bindings, host_vars)
}

/// Build the `ɵɵHostDirectivesFeature(...)` call from a `hostDirectives` array
/// in the partial declaration.
///
/// The runtime accepts either bare class references or
/// `{ directive, inputs?, outputs? }` objects. `inputs` / `outputs` must be
/// flat-pair arrays (`[publicName1, privateName1, publicName2, privateName2]`)
/// — the partial-declaration form encodes pairs as `'public: private'` colon
/// strings, so we run the array through `transform_host_directives_array` to
/// split colon strings into the runtime's flat-pair shape. Without this the
/// `bindingArrayToMap` consumer reads the colon strings as a single key with
/// an `undefined` value and the input/output remapping silently drops.
pub(crate) fn build_host_directives_feature(
    obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
) -> Option<String> {
    let arr_text = metadata::get_source_text(obj, "hostDirectives", source)?;
    let feature = if ng_import.is_empty() {
        "\u{0275}\u{0275}HostDirectivesFeature".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}HostDirectivesFeature")
    };
    let normalised = host_codegen::transform_host_directives_array(arr_text)
        .unwrap_or_else(|| arr_text.to_string());
    Some(format!("{feature}({normalised})"))
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

/// Build a `contentQueries` function from the declare-format `queries` array.
///
/// Transforms: `[{ propertyName: "links", predicate: RouterLink, descendants: true }]`
/// Into: `function(rf, ctx, directiveIndex) { if (rf & 1) { ɵɵcontentQuery(directiveIndex, RouterLink, 5); } if (rf & 2) { let _t; ɵɵqueryRefresh(_t = ɵɵloadQuery()) && (ctx.links = _t); } }`
pub(crate) fn build_content_queries(
    queries_source: &str,
    ng_import: &str,
    source: &str,
    obj: &oxc_ast::ast::ObjectExpression<'_>,
) -> Option<String> {
    let queries = parse_query_descriptors(queries_source, source, obj)?;
    if queries.is_empty() {
        return None;
    }

    let (cq, refresh, load) = if ng_import.is_empty() {
        (
            "\u{0275}\u{0275}contentQuery".to_string(),
            "\u{0275}\u{0275}queryRefresh".to_string(),
            "\u{0275}\u{0275}loadQuery".to_string(),
        )
    } else {
        (
            format!("{ng_import}.\u{0275}\u{0275}contentQuery"),
            format!("{ng_import}.\u{0275}\u{0275}queryRefresh"),
            format!("{ng_import}.\u{0275}\u{0275}loadQuery"),
        )
    };

    let mut create_stmts = Vec::new();
    let mut update_stmts = Vec::new();

    for q in &queries {
        let flags = compute_query_flags(q);
        let read_arg = if let Some(ref read) = q.read {
            format!(", {read}")
        } else {
            String::new()
        };
        create_stmts.push(format!(
            "{cq}(directiveIndex, {}, {flags}{read_arg});",
            q.predicate
        ));
        let assign_expr = if q.first {
            format!("ctx.{} = _t.first", q.property_name)
        } else {
            format!("ctx.{} = _t", q.property_name)
        };
        update_stmts.push(format!(
            "let _t; {refresh}(_t = {load}()) && ({assign_expr});"
        ));
    }

    let mut body = String::from("if (rf & 1) { ");
    for s in &create_stmts {
        body.push_str(s);
        body.push(' ');
    }
    body.push_str("} if (rf & 2) { ");
    for s in &update_stmts {
        body.push_str(s);
        body.push(' ');
    }
    body.push('}');

    Some(format!("function(rf, ctx, directiveIndex) {{ {body} }}"))
}

/// Build a `viewQuery` function from the declare-format `viewQueries` array.
pub(crate) fn build_view_queries(
    queries_source: &str,
    ng_import: &str,
    source: &str,
    obj: &oxc_ast::ast::ObjectExpression<'_>,
) -> Option<String> {
    let queries = parse_query_descriptors(queries_source, source, obj)?;
    if queries.is_empty() {
        return None;
    }

    let (vq, refresh, load) = if ng_import.is_empty() {
        (
            "\u{0275}\u{0275}viewQuery".to_string(),
            "\u{0275}\u{0275}queryRefresh".to_string(),
            "\u{0275}\u{0275}loadQuery".to_string(),
        )
    } else {
        (
            format!("{ng_import}.\u{0275}\u{0275}viewQuery"),
            format!("{ng_import}.\u{0275}\u{0275}queryRefresh"),
            format!("{ng_import}.\u{0275}\u{0275}loadQuery"),
        )
    };

    let mut create_stmts = Vec::new();
    let mut update_stmts = Vec::new();

    for q in &queries {
        let flags = compute_query_flags(q);
        let read_arg = if let Some(ref read) = q.read {
            format!(", {read}")
        } else {
            String::new()
        };
        create_stmts.push(format!("{vq}({}, {flags}{read_arg});", q.predicate));
        let assign_expr = if q.first {
            format!("ctx.{} = _t.first", q.property_name)
        } else {
            format!("ctx.{} = _t", q.property_name)
        };
        update_stmts.push(format!(
            "let _t; {refresh}(_t = {load}()) && ({assign_expr});"
        ));
    }

    let mut body = String::from("if (rf & 1) { ");
    for s in &create_stmts {
        body.push_str(s);
        body.push(' ');
    }
    body.push_str("} if (rf & 2) { ");
    for s in &update_stmts {
        body.push_str(s);
        body.push(' ');
    }
    body.push('}');

    Some(format!("function(rf, ctx) {{ {body} }}"))
}

/// A parsed query descriptor.
struct QueryDescriptor {
    property_name: String,
    predicate: String,
    descendants: bool,
    is_static: bool,
    read: Option<String>,
    first: bool,
}

/// Compute the flags integer for a query.
fn compute_query_flags(q: &QueryDescriptor) -> u32 {
    let mut flags: u32 = 0;
    if q.descendants {
        flags |= 1; // QueryFlags.descendants
    }
    if q.is_static {
        flags |= 2; // QueryFlags.isStatic
    }
    if !q.first {
        flags |= 4; // QueryFlags.emitDistinctChangesOnly (for QueryList)
    }
    flags
}

/// Parse query descriptors from the raw source text of the `queries` array.
///
/// Uses oxc to parse the array literal and extract each query's fields.
fn parse_query_descriptors(
    _queries_source: &str,
    source: &str,
    obj: &oxc_ast::ast::ObjectExpression<'_>,
) -> Option<Vec<QueryDescriptor>> {
    use oxc_ast::ast::*;

    // Find the queries/viewQueries property
    let queries_arr = obj.properties.iter().find_map(|p| {
        if let ObjectPropertyKind::ObjectProperty(prop) = p {
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
                _ => None,
            };
            if key_name == Some("queries") || key_name == Some("viewQueries") {
                if let Expression::ArrayExpression(arr) = &prop.value {
                    return Some(arr.as_ref());
                }
            }
        }
        None
    })?;

    let mut descriptors = Vec::new();
    for elem in &queries_arr.elements {
        if let ArrayExpressionElement::ObjectExpression(desc_obj) = elem {
            let property_name = metadata::get_string_prop(desc_obj, "propertyName")?;
            let predicate = metadata::get_source_text(desc_obj, "predicate", source)
                .unwrap_or("null")
                .to_string();
            let descendants = metadata::get_bool_prop(desc_obj, "descendants").unwrap_or(false);
            let is_static = metadata::get_bool_prop(desc_obj, "static").unwrap_or(false);
            let first = metadata::get_bool_prop(desc_obj, "first").unwrap_or(false);
            let read = metadata::get_source_text(desc_obj, "read", source).map(|s| s.to_string());

            descriptors.push(QueryDescriptor {
                property_name,
                predicate,
                descendants,
                is_static,
                read,
                first,
            });
        }
    }

    Some(descriptors)
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
    fn test_directive_export_as_string_array() {
        // `ɵɵngDeclareDirective` emits `exportAs` as a string array
        // (e.g. @angular/forms' NgForm).  The linker must transfer it into
        // the emitted `ɵɵdefineDirective` call, otherwise runtime
        // ref lookups with `#ref="exportAsName"` fail with NG0301.
        let result =
            parse_and_transform("{ type: NgForm, selector: 'form', exportAs: ['ngForm'] }");
        assert!(
            result.contains("exportAs: [\"ngForm\"]"),
            "expected exportAs emitted from array form: {result}"
        );
    }

    #[test]
    fn test_directive_export_as_string_array_multiple() {
        let result =
            parse_and_transform("{ type: MyDir, selector: '[myDir]', exportAs: ['a', 'b'] }");
        assert!(
            result.contains("exportAs: [\"a\", \"b\"]"),
            "expected multi-name exportAs array: {result}"
        );
    }

    #[test]
    fn test_directive_export_as_string_legacy() {
        // Legacy comma-separated string form must still work.
        let result =
            parse_and_transform("{ type: MyDir, selector: '[myDir]', exportAs: 'foo,bar' }");
        assert!(
            result.contains("exportAs: [\"foo\", \"bar\"]"),
            "expected comma-split string exportAs: {result}"
        );
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
    fn test_host_binding_attr() {
        let result = parse_and_transform(
            "{ type: RouterLink, selector: 'a[routerLink]', host: { properties: { 'attr.href': 'href' } } }",
        );
        assert!(
            result.contains("i0.\u{0275}\u{0275}attribute(\"href\", ctx.href)"),
            "expected ɵɵattribute call for attr.href, got: {result}"
        );
        assert!(!result.contains("\u{0275}\u{0275}property(\"attr.href\""));
        assert!(result.contains("hostVars: 1"));
    }

    #[test]
    fn test_host_binding_attr_mixed() {
        let result = parse_and_transform(
            "{ type: MyDir, selector: '[myDir]', host: { properties: { 'attr.href': 'href', 'class.active': 'isActive', 'disabled': 'isDisabled' } } }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}attribute(\"href\", ctx.href)"));
        assert!(result.contains("i0.\u{0275}\u{0275}classProp(\"active\", ctx.isActive)"));
        assert!(result.contains("i0.\u{0275}\u{0275}property(\"disabled\", ctx.isDisabled)"));
        // attr (1) + class (2) + property (1) = 4
        assert!(
            result.contains("hostVars: 4"),
            "expected hostVars: 4, got: {result}"
        );
    }

    /// `style.X.unit` must split off the unit so the runtime gets a
    /// 3-arg `ɵɵstyleProp(propName, expr, suffix)` — emitting
    /// `ɵɵstyleProp("X.unit", expr)` would set `width.px` as the property
    /// name and the unit suffix would never reach the renderer.
    #[test]
    fn test_host_binding_style_with_unit() {
        let result = parse_and_transform(
            "{ type: MyDir, selector: '[myDir]', host: { properties: { 'style.width.px': 'width' } } }",
        );
        assert!(
            result.contains("i0.\u{0275}\u{0275}styleProp(\"width\", ctx.width, \"px\")"),
            "expected style.X.unit to split off the unit; got: {result}"
        );
        assert!(result.contains("hostVars: 2"));
    }

    // ---- AOT ↔ linker parity for `@HostListener` / `@HostBinding` (issue #58) ----
    //
    // Compiles a directive with each decorator target form via the AOT path
    // and asserts the same Ivy instructions appear in the linker's output for
    // an equivalent `host: { listeners, properties }` partial declaration.

    use ngc_template_compiler::compile_all_decorators;
    use std::io::Write;

    /// Compile a `@Directive` source through the AOT pipeline. We have to
    /// hand the source to `compile_all_decorators` via a tempfile because
    /// the per-string entry point only knows how to recognise `@Component`.
    fn aot_compile(source: &str) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("host.directive.ts");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(source.as_bytes()).expect("write");
        drop(f);
        let mut out = compile_all_decorators(&[path]).expect("compile");
        let compiled = out.pop().expect("one file");
        assert!(compiled.compiled, "expected AOT to compile the directive");
        compiled.source
    }

    #[test]
    fn parity_host_listener_event_only() {
        let aot = aot_compile(
            "import { Directive, HostListener } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir { @HostListener('click') onClick() {} }\n",
        );
        let linker =
            parse_and_transform("{ type: HostDir, selector: '[appHost]', isStandalone: true, host: { listeners: { 'click': 'onClick()' } } }");

        assert!(aot.contains(
            "\u{0275}\u{0275}listener(\"click\", function($event) { return ctx.onClick(); })"
        ));
        assert!(linker.contains(
            "i0.\u{0275}\u{0275}listener(\"click\", function($event) { return ctx.onClick(); })"
        ));
        assert!(aot.contains("if (rf & 1)"));
        assert!(linker.contains("if (rf & 1)"));
    }

    #[test]
    fn parity_host_binding_bare() {
        let aot = aot_compile(
            "import { Directive, HostBinding } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir { @HostBinding('disabled') isDisabled = false; }\n",
        );
        let linker = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, host: { properties: { 'disabled': 'isDisabled' } } }",
        );

        assert!(aot.contains("\u{0275}\u{0275}property(\"disabled\", ctx.isDisabled)"));
        assert!(linker.contains("i0.\u{0275}\u{0275}property(\"disabled\", ctx.isDisabled)"));
        assert!(aot.contains("hostVars: 1"));
        assert!(linker.contains("hostVars: 1"));
    }

    #[test]
    fn parity_host_binding_attr() {
        let aot = aot_compile(
            "import { Directive, HostBinding } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir { @HostBinding('attr.aria-label') label = 'host'; }\n",
        );
        let linker = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, host: { properties: { 'attr.aria-label': 'label' } } }",
        );

        assert!(aot.contains("\u{0275}\u{0275}attribute(\"aria-label\", ctx.label)"));
        assert!(linker.contains("i0.\u{0275}\u{0275}attribute(\"aria-label\", ctx.label)"));
        assert!(aot.contains("hostVars: 1"));
        assert!(linker.contains("hostVars: 1"));
    }

    #[test]
    fn parity_host_binding_class() {
        let aot = aot_compile(
            "import { Directive, HostBinding } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir { @HostBinding('class.active') isActive = true; }\n",
        );
        let linker = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, host: { properties: { 'class.active': 'isActive' } } }",
        );

        assert!(aot.contains("\u{0275}\u{0275}classProp(\"active\", ctx.isActive)"));
        assert!(linker.contains("i0.\u{0275}\u{0275}classProp(\"active\", ctx.isActive)"));
        assert!(aot.contains("hostVars: 2"));
        assert!(linker.contains("hostVars: 2"));
    }

    #[test]
    fn parity_host_binding_style_simple() {
        let aot = aot_compile(
            "import { Directive, HostBinding } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir { @HostBinding('style.color') color = 'red'; }\n",
        );
        let linker = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, host: { properties: { 'style.color': 'color' } } }",
        );

        assert!(aot.contains("\u{0275}\u{0275}styleProp(\"color\", ctx.color)"));
        assert!(linker.contains("i0.\u{0275}\u{0275}styleProp(\"color\", ctx.color)"));
        assert!(aot.contains("hostVars: 2"));
        assert!(linker.contains("hostVars: 2"));
    }

    #[test]
    fn parity_host_binding_style_with_unit() {
        let aot = aot_compile(
            "import { Directive, HostBinding } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir { @HostBinding('style.width.px') width = 100; }\n",
        );
        let linker = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, host: { properties: { 'style.width.px': 'width' } } }",
        );

        assert!(aot.contains("\u{0275}\u{0275}styleProp(\"width\", ctx.width, \"px\")"));
        assert!(linker.contains("i0.\u{0275}\u{0275}styleProp(\"width\", ctx.width, \"px\")"));
        assert!(aot.contains("hostVars: 2"));
        assert!(linker.contains("hostVars: 2"));
    }

    #[test]
    fn host_directives_object_form_emits_feature() {
        // Partial declarations encode `inputs` / `outputs` as `'public: private'`
        // colon strings (and bare names as identity). The runtime's
        // `bindingArrayToMap` reads the array as flat pairs, so the linker must
        // split each colon string into a `'public', 'private'` pair (and emit
        // `'name', 'name'` for bare entries) before wrapping the whole thing in
        // `ɵɵHostDirectivesFeature(...)`. Without this conversion the runtime
        // sees a single key with `undefined` value and silently drops the
        // remapping.
        let result = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, hostDirectives: [{ directive: ChildDir, inputs: ['childInput', 'aliased: localName'], outputs: ['childOutput'] }] }",
        );
        assert!(
            result.contains("i0.\u{0275}\u{0275}HostDirectivesFeature(["),
            "expected ɵɵHostDirectivesFeature wrapper, got: {result}"
        );
        assert!(
            result.contains("directive: ChildDir"),
            "composed directive class reference must be preserved verbatim: {result}"
        );
        // Bare name → identity pair.
        assert!(
            result.contains("inputs: ['childInput', 'childInput'"),
            "bare input name must become identity pair: {result}"
        );
        // Colon syntax → flat pair (left, right).
        assert!(
            result.contains("'aliased', 'localName'"),
            "colon-syntax remapping must be split into a flat pair: {result}"
        );
        // Outputs follow the same shape — bare name → identity pair.
        assert!(
            result.contains("outputs: ['childOutput', 'childOutput']"),
            "bare output name must become identity pair: {result}"
        );
        // The colon-string form must NOT survive into the runtime call.
        assert!(
            !result.contains("'aliased: localName'"),
            "raw colon-syntax string must not reach the runtime: {result}"
        );
        assert!(result.contains("features: ["));
    }

    #[test]
    fn host_directives_bare_form_emits_feature() {
        // Even though partial declarations always emit the object form, the
        // runtime accepts bare class references too. Linker should pass the
        // array source through verbatim — `ɵɵHostDirectivesFeature` handles
        // both shapes.
        let result = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, hostDirectives: [BareChildDir] }",
        );
        assert!(
            result.contains("i0.\u{0275}\u{0275}HostDirectivesFeature([BareChildDir])"),
            "expected bare class reference inside feature call, got: {result}"
        );
    }

    #[test]
    fn host_directives_combines_with_providers_feature() {
        // When both `providers` and `hostDirectives` are present, both feature
        // calls must appear in the same `features` array. Order doesn't matter
        // for runtime correctness, but both must be present.
        let result = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, providers: [SomeService], hostDirectives: [ChildDir] }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}ProvidersFeature"));
        assert!(result.contains("i0.\u{0275}\u{0275}HostDirectivesFeature"));
    }

    #[test]
    fn parity_host_mixed_listener_and_bindings() {
        let aot = aot_compile(
            "import { Directive, HostListener, HostBinding } from '@angular/core';\n\
             @Directive({ selector: '[appHost]', standalone: true })\n\
             export class HostDir {\n\
               @HostBinding('attr.aria-label') label = 'host';\n\
               @HostBinding('class.active') isActive = true;\n\
               @HostBinding('disabled') isDisabled = false;\n\
               @HostListener('click', ['$event']) onClick($event: Event) {}\n\
             }\n",
        );
        let linker = parse_and_transform(
            "{ type: HostDir, selector: '[appHost]', isStandalone: true, \
             host: { properties: { 'attr.aria-label': 'label', 'class.active': 'isActive', 'disabled': 'isDisabled' }, \
                     listeners: { 'click': 'onClick($event)' } } }",
        );

        // attr (1) + class (2) + property (1) = 4 — listeners contribute 0.
        assert!(aot.contains("hostVars: 4"));
        assert!(linker.contains("hostVars: 4"));

        for must_contain in [
            "if (rf & 1)",
            "if (rf & 2)",
            "\u{0275}\u{0275}listener(\"click\"",
            "\u{0275}\u{0275}attribute(\"aria-label\"",
            "\u{0275}\u{0275}classProp(\"active\"",
            "\u{0275}\u{0275}property(\"disabled\"",
        ] {
            assert!(
                aot.contains(must_contain),
                "AOT missing {must_contain:?}; got:\n{aot}"
            );
            assert!(
                linker.contains(must_contain),
                "linker missing {must_contain:?}; got:\n{linker}"
            );
        }
    }
}
