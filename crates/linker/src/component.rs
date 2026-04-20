//! Transform `ɵɵngDeclareComponent` → `ɵɵdefineComponent`.
//!
//! This is the most complex transformation — it compiles the component's template
//! string into a template function using the template compiler, then combines it
//! with the directive-level metadata (selectors, inputs, outputs, host bindings).

use std::path::Path;

use ngc_diagnostics::NgcResult;
use oxc_ast::ast::{
    ArrayExpressionElement, Expression, ObjectExpression, ObjectPropertyKind, PropertyKey,
};
use oxc_span::GetSpan;

use crate::metadata;
use crate::selector;

/// Transform a `ɵɵngDeclareComponent` call into a `ɵɵdefineComponent` call.
pub fn transform(
    obj: &ObjectExpression<'_>,
    source: &str,
    ng_import: &str,
    file_path: &Path,
) -> NgcResult<String> {
    let define_fn = if ng_import.is_empty() {
        "\u{0275}\u{0275}defineComponent".to_string()
    } else {
        format!("{ng_import}.\u{0275}\u{0275}defineComponent")
    };

    let type_text = metadata::get_source_text(obj, "type", source).unwrap_or("Unknown");
    let type_name =
        metadata::get_identifier_prop(obj, "type").unwrap_or_else(|| type_text.to_string());

    let mut props = Vec::new();
    props.push(format!("type: {type_text}"));

    // Parse selector into Angular array format
    if let Some(sel) = metadata::get_string_prop(obj, "selector") {
        props.push(format!("selectors: {}", selector::parse_selector(&sel)));
    }

    // Inputs (reuse directive logic)
    if let Some(inputs) = build_inputs(obj, source) {
        props.push(format!("inputs: {inputs}"));
    }

    // Outputs
    if let Some(outputs) = build_outputs(obj, source) {
        props.push(format!("outputs: {outputs}"));
    }

    // Standalone
    let is_standalone = metadata::get_bool_prop(obj, "isStandalone") == Some(true)
        || metadata::get_bool_prop(obj, "standalone") == Some(true);
    if is_standalone {
        props.push("standalone: true".to_string());
    }

    // Features — only emit features that exist in the Angular runtime.
    // ɵɵStandaloneFeature was removed in Angular 19+; standalone is handled via property.
    //
    // `ɵɵProvidersFeature(providers[, viewProviders])` wires component-level
    // providers into the node injector at instantiation time.
    let mut features = Vec::new();
    if metadata::get_bool_prop(obj, "usesInheritance") == Some(true) {
        let feat = if ng_import.is_empty() {
            "\u{0275}\u{0275}InheritDefinitionFeature".to_string()
        } else {
            format!("{ng_import}.\u{0275}\u{0275}InheritDefinitionFeature")
        };
        features.push(feat);
    }
    let providers_src = metadata::get_source_text(obj, "providers", source);
    let view_providers_src = metadata::get_source_text(obj, "viewProviders", source);
    if let Some(providers) = providers_src {
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
    if !features.is_empty() {
        props.push(format!("features: [{}]", features.join(", ")));
    }

    // Template compilation
    let mut child_fns_code: Vec<String> = Vec::new();
    let mut extra_ivy_imports: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    if let Some(template_str) = metadata::get_string_prop(obj, "template") {
        let template_ok = match compile_template(&template_str, &type_name, file_path) {
            Ok(tpl) => {
                let template_fn = tpl.template_function;
                let decls = tpl.decls;
                let vars = tpl.vars;
                let child_fns = tpl.child_template_functions;
                let ivy_imports = tpl.ivy_imports;
                let consts = tpl.consts;
                // Validate that the compiled template is valid JavaScript
                // (the template parser may "succeed" on unsupported syntax like @let
                // but produce output with literal newlines inside string literals)
                if is_valid_js_function(&template_fn) {
                    if !consts.is_empty() {
                        props.push(format!("consts: [{}]", consts.join(", ")));
                    }
                    props.push(format!("decls: {decls}"));
                    props.push(format!("vars: {vars}"));
                    props.push(format!("template: {template_fn}"));
                    child_fns_code = child_fns;
                    extra_ivy_imports = ivy_imports;
                    true
                } else {
                    tracing::warn!(
                        path = %file_path.display(),
                        "compiled template produced invalid JS, using empty template"
                    );
                    false
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %file_path.display(),
                    error = %e,
                    "template compilation failed for npm component, using empty template"
                );
                false
            }
        };
        if !template_ok {
            props.push("decls: 0".to_string());
            props.push("vars: 0".to_string());
            props.push(format!(
                "template: function {type_name}_Template(rf, ctx) {{}}"
            ));
        }
    } else {
        // No template — empty
        props.push("decls: 0".to_string());
        props.push("vars: 0".to_string());
        props.push(format!(
            "template: function {type_name}_Template(rf, ctx) {{}}"
        ));
    }

    // Dependencies — extract type references from the dependencies array
    if let Some(deps_text) = build_dependencies(obj, source) {
        props.push(format!("dependencies: {deps_text}"));
    }

    // Host bindings
    if let Some(host_obj) = metadata::get_object_prop(obj, "host") {
        let (host_attrs, host_bindings, host_vars) =
            crate::directive::build_host_bindings(host_obj, source, ng_import);
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

    // Styles
    if let Some(styles) = metadata::get_source_text(obj, "styles", source) {
        props.push(format!("styles: {styles}"));
    }

    // Encapsulation
    if let Some(encapsulation) = metadata::get_source_text(obj, "encapsulation", source) {
        props.push(format!("encapsulation: {encapsulation}"));
    }

    // Change detection
    if let Some(cd) = metadata::get_source_text(obj, "changeDetection", source) {
        props.push(format!("changeDetection: {cd}"));
    }

    // Content queries
    if let Some(queries) = metadata::get_source_text(obj, "queries", source) {
        if let Some(content_queries_fn) =
            crate::directive::build_content_queries(queries, ng_import, source, obj)
        {
            props.push(format!("contentQueries: {content_queries_fn}"));
        }
    }

    // View queries
    if let Some(view_queries) = metadata::get_source_text(obj, "viewQueries", source) {
        if let Some(view_query_fn) =
            crate::directive::build_view_queries(view_queries, ng_import, source, obj)
        {
            props.push(format!("viewQuery: {view_query_fn}"));
        }
    }

    let define_call = format!("{define_fn}({{ {} }})", props.join(", "));

    // If the template produced child template functions (e.g. for @if/@for blocks),
    // wrap everything in an IIFE so the functions are in scope when the template runs.
    if child_fns_code.is_empty() && extra_ivy_imports.is_empty() {
        Ok(define_call)
    } else {
        let fns = child_fns_code.join("\n");
        // Add var declarations for new Ivy symbols (e.g. ɵɵdeclareLet, ɵɵstoreLet)
        // that weren't in the original npm source. Use the ng_import prefix (e.g. i0).
        let mut var_decls = String::new();
        if !ng_import.is_empty() {
            for sym in &extra_ivy_imports {
                var_decls.push_str(&format!("var {sym} = {ng_import}.{sym};\n"));
            }
        }
        Ok(format!(
            "(function() {{ {var_decls}{fns}\nreturn {define_call}; }})()"
        ))
    }
}

/// Validate that a generated template function is valid JavaScript.
///
/// The template parser may produce output with unsupported Angular syntax
/// (like `@let`) treated as raw text, resulting in multi-line string literals
/// which are invalid JS.
fn is_valid_js_function(code: &str) -> bool {
    let wrapped = format!("var x = {code};");
    let alloc = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&alloc, &wrapped, oxc_span::SourceType::mjs()).parse();
    parsed.errors.is_empty()
}

/// Compile a template string into a template function using the template compiler.
///
/// Compile a template and return the template function output.
fn compile_template(
    template: &str,
    component_name: &str,
    file_path: &Path,
) -> NgcResult<ngc_template_compiler::TemplateFnOutput> {
    use ngc_template_compiler::{generate_template_fn, TemplateMetadata};

    let meta = TemplateMetadata {
        class_name: component_name.to_string(),
        selector: String::new(),
        standalone: false,
        imports_source: None,
        styles_source: None,
    };

    generate_template_fn(template, &meta, file_path)
}

/// Build the `dependencies` array from the declare format.
///
/// The declare format has objects like `{ kind: "directive", type: MyDir, selector: "..." }`.
/// The runtime format just needs the type references: `[MyDir, MyPipe, ...]`.
fn build_dependencies(obj: &ObjectExpression<'_>, source: &str) -> Option<String> {
    // Find the dependencies array
    let deps_arr = obj.properties.iter().find_map(|p| {
        if let ObjectPropertyKind::ObjectProperty(prop) = p {
            if matches!(&prop.key, PropertyKey::StaticIdentifier(id) if id.name.as_str() == "dependencies")
            {
                if let Expression::ArrayExpression(arr) = &prop.value {
                    return Some(arr.as_ref());
                }
            }
        }
        None
    })?;

    let mut types = Vec::new();
    for element in &deps_arr.elements {
        match element {
            ArrayExpressionElement::ObjectExpression(dep_obj) => {
                // Extract the type reference from each dependency descriptor
                if let Some(type_ref) = metadata::get_source_text(dep_obj, "type", source) {
                    types.push(type_ref.to_string());
                }
            }
            _ => {
                // Direct reference (not a descriptor object)
                let span = element.span();
                types.push(source[span.start as usize..span.end as usize].to_string());
            }
        }
    }

    if types.is_empty() {
        None
    } else {
        Some(format!("[{}]", types.join(", ")))
    }
}

/// Build inputs from the declare format (shared with directive).
fn build_inputs(obj: &ObjectExpression<'_>, source: &str) -> Option<String> {
    let inputs_obj = metadata::get_object_prop(obj, "inputs")?;

    let mut entries = Vec::new();
    for prop in &inputs_obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            let key = prop_key_text(&p.key, source);

            match &p.value {
                Expression::StringLiteral(s) => {
                    let alias = s.value.as_str();
                    if alias == key {
                        entries.push(format!("{key}: '{key}'"));
                    } else {
                        entries.push(format!("{key}: [0, '{alias}', '{key}']"));
                    }
                }
                Expression::ObjectExpression(input_obj) => {
                    let alias = metadata::get_string_prop(input_obj, "alias")
                        .unwrap_or_else(|| key.clone());
                    let required = metadata::get_bool_prop(input_obj, "required") == Some(true);
                    let flags = if required { 1 } else { 0 };
                    if required || alias != key {
                        entries.push(format!("{key}: [{flags}, '{alias}', '{key}']"));
                    } else {
                        entries.push(format!("{key}: '{key}'"));
                    }
                }
                _ => {
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

/// Build outputs from the declare format.
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
    fn test_component_basic() {
        let result = parse_and_transform(
            "{ type: MyComp, selector: 'my-comp', isStandalone: true, template: '<div>hello</div>' }",
        );
        assert!(result.contains("i0.\u{0275}\u{0275}defineComponent"));
        assert!(result.contains("type: MyComp"));
        assert!(result.contains("selectors: [['my-comp']]"));
        assert!(result.contains("standalone: true"));
        assert!(result.contains("template:"));
    }

    #[test]
    fn test_component_no_template() {
        let result = parse_and_transform("{ type: MyComp, selector: 'my-comp' }");
        assert!(result.contains("decls: 0"));
        assert!(result.contains("vars: 0"));
    }
}
