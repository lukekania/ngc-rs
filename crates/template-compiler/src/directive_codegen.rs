//! AOT codegen for `@Directive` decorators.
//!
//! Generates `ɵfac` (factory) and `ɵdir` (`ɵɵdefineDirective`) static fields.
//!
//! ## Example
//! ```text
//! // Input:
//! @Directive({ selector: '[appHighlight]', standalone: true })
//! export class HighlightDirective {}
//!
//! // Output:
//! export class HighlightDirective {
//!   static ɵfac = function HighlightDirective_Factory(t: any) { return new (t || HighlightDirective)(); };
//!   static ɵdir = ɵɵdefineDirective({ type: HighlightDirective, selectors: [['', 'appHighlight', '']], standalone: true });
//! }
//! ```

use std::collections::BTreeSet;

use ngc_diagnostics::NgcResult;

use crate::codegen::IvyOutput;
use crate::extract::{ExtractedDirective, HostBindingSpec, HostListenerSpec};
use crate::factory_codegen;
use crate::host_codegen;
use crate::selector;

/// Generate Ivy output for a `@Directive` decorator.
pub fn generate_directive_ivy(extracted: &ExtractedDirective) -> NgcResult<IvyOutput> {
    let name = &extracted.class_name;
    let mut ivy_imports = BTreeSet::new();

    ivy_imports.insert("\u{0275}\u{0275}defineDirective".to_string());

    // Generate factory with DI
    let (factory_code, inject_imports) =
        factory_codegen::generate_factory(name, &extracted.constructor_params);
    for imp in inject_imports {
        ivy_imports.insert(imp);
    }

    // Build ɵdir definition
    let mut props = Vec::new();
    props.push(format!("type: {name}"));

    if let Some(ref sel) = extracted.selector {
        props.push(format!("selectors: {}", selector::parse_selector(sel)));
    }

    // Decorator-literal `inputs:` / `outputs:` come through verbatim
    // as raw source. Signal-API class fields (`input()`, `output()`,
    // `model()`, view/content queries) are merged in via
    // `compile_signal_members`. When both shapes coexist on a directive
    // the literal wins as the base and the signal entries are appended
    // — Angular's compiler does the same.
    let signal_members = crate::signal_codegen::compile_signal_members(
        &[],
        &extracted.signal_inputs,
        &extracted.signal_outputs,
        &extracted.signal_models,
        &extracted.signal_queries,
    );
    for sym in &signal_members.ivy_imports {
        ivy_imports.insert(sym.clone());
    }

    let inputs_block = build_inputs_block(
        extracted.inputs_source.as_deref(),
        &signal_members.inputs_entries,
    );
    if let Some(b) = inputs_block {
        props.push(b);
    }
    let outputs_block = build_outputs_block(
        extracted.outputs_source.as_deref(),
        &signal_members.outputs_entries,
    );
    if let Some(b) = outputs_block {
        props.push(b);
    }
    if let Some(ref vq) = signal_members.view_query_prop {
        props.push(vq.clone());
    }
    if let Some(ref cq) = signal_members.content_queries_prop {
        props.push(cq.clone());
    }

    let host_props = build_host_props(&extracted.host_listeners, &extracted.host_bindings);
    for imp in &host_props.ivy_imports {
        ivy_imports.insert(imp.clone());
    }
    if let Some(host_vars) = host_props.host_vars_prop {
        props.push(host_vars);
    }
    if let Some(host_bindings) = host_props.host_bindings_prop {
        props.push(host_bindings);
    }

    if let Some(ref export_as) = extracted.export_as {
        let parts: Vec<&str> = export_as.split(',').map(|s| s.trim()).collect();
        let arr = parts
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        props.push(format!("exportAs: [{arr}]"));
    }

    if extracted.standalone {
        props.push("standalone: true".to_string());
    }

    // hostDirectives composition (Angular 15+). Wrap the source array in a
    // `ɵɵHostDirectivesFeature(...)` call inside `features` so the runtime
    // instantiates the composed directives on the host element. The array is
    // normalised first: decorator-form `inputs`/`outputs` use `'public: private'`
    // colon syntax, but the runtime expects flat-pair arrays.
    if let Some(ref host_dirs_src) = extracted.host_directives_source {
        ivy_imports.insert("\u{0275}\u{0275}HostDirectivesFeature".to_string());
        let normalised = host_codegen::transform_host_directives_array(host_dirs_src)
            .unwrap_or_else(|| host_dirs_src.clone());
        props.push(format!(
            "features: [\u{0275}\u{0275}HostDirectivesFeature({normalised})]"
        ));
    }

    let define_code = format!(
        "static \u{0275}dir = \u{0275}\u{0275}defineDirective({{ {} }})",
        props.join(", ")
    );

    Ok(IvyOutput {
        factory_code,
        static_fields: vec![define_code],
        child_template_functions: Vec::new(),
        ivy_imports,
        consts: Vec::new(),
    })
}

/// Result of compiling decorator-extracted host metadata into the
/// `hostVars`/`hostBindings` properties of `ɵɵdefineDirective`/`ɵɵdefineComponent`.
pub(crate) struct CompiledHostProps {
    /// `hostVars: N` property string, only set when host_vars > 0.
    pub host_vars_prop: Option<String>,
    /// `hostBindings: function(rf, ctx) { ... }` property string.
    pub host_bindings_prop: Option<String>,
    /// Ivy instruction symbols referenced by the emitted statements
    /// (e.g. `ɵɵlistener`, `ɵɵclassProp`). The caller must add these to the
    /// import set so the rewrite step pulls them from `@angular/core`.
    pub ivy_imports: BTreeSet<String>,
}

/// Compile `@HostListener` and `@HostBinding` extractions into `hostVars` and
/// `hostBindings` property strings ready to splice into a define call.
///
/// Produces no output when both lists are empty. AOT codegen passes empty
/// `ng_import` (decorators emit unprefixed `ɵɵlistener` / `ɵɵproperty`
/// references); the linker passes its own `ng_import` namespace.
pub(crate) fn build_host_props(
    listeners: &[HostListenerSpec],
    bindings: &[HostBindingSpec],
) -> CompiledHostProps {
    let mut listener_stmts = Vec::with_capacity(listeners.len());
    let mut imports = BTreeSet::new();
    for l in listeners {
        listener_stmts.push(host_codegen::dispatch_listener(
            &l.event,
            &l.handler_expression,
            "",
        ));
        imports.insert("\u{0275}\u{0275}listener".to_string());
    }

    let mut binding_stmts = Vec::with_capacity(bindings.len());
    let mut host_vars: u32 = 0;
    for b in bindings {
        let (stmt, vars) = host_codegen::dispatch_property_binding(&b.target, &b.property_name, "");
        binding_stmts.push(stmt);
        host_vars += vars;
        imports.insert(host_instruction_for(&b.target).to_string());
    }

    let host_bindings_fn =
        host_codegen::build_host_bindings_function(&binding_stmts, &listener_stmts);

    CompiledHostProps {
        host_vars_prop: (host_vars > 0).then(|| format!("hostVars: {host_vars}")),
        host_bindings_prop: host_bindings_fn.map(|f| format!("hostBindings: {f}")),
        ivy_imports: imports,
    }
}

/// Combine the decorator-literal `inputs:` source (if any) with the
/// signal-API entries produced by `signal_codegen` into a single
/// `inputs: { ... }` property string.
///
/// When both are empty we return `None` so the caller can skip the
/// property altogether — Angular's runtime treats a missing `inputs`
/// the same as an empty map, and the smaller emit reads cleaner in
/// codegen golden tests.
///
/// Splicing the two together by trimming the literal's outer braces is
/// crude but sufficient: the decorator literal is always an
/// `ObjectExpression` (the extractor only fills `inputs_source` from
/// an object value), so matching the leading `{` / trailing `}` is
/// safe.
fn build_inputs_block(literal_src: Option<&str>, signal_entries: &[String]) -> Option<String> {
    match (literal_src, signal_entries.is_empty()) {
        (None, true) => None,
        (Some(src), true) => Some(format!("inputs: {src}")),
        (None, false) => Some(format!("inputs: {{ {} }}", signal_entries.join(", "))),
        (Some(src), false) => {
            let inner = src
                .trim()
                .trim_start_matches('{')
                .trim_end_matches('}')
                .trim();
            let combined = if inner.is_empty() {
                signal_entries.join(", ")
            } else {
                format!("{inner}, {}", signal_entries.join(", "))
            };
            Some(format!("inputs: {{ {combined} }}"))
        }
    }
}

/// Same idea as [`build_inputs_block`], for the `outputs` map.
fn build_outputs_block(literal_src: Option<&str>, signal_entries: &[String]) -> Option<String> {
    match (literal_src, signal_entries.is_empty()) {
        (None, true) => None,
        (Some(src), true) => Some(format!("outputs: {src}")),
        (None, false) => Some(format!("outputs: {{ {} }}", signal_entries.join(", "))),
        (Some(src), false) => {
            let inner = src
                .trim()
                .trim_start_matches('{')
                .trim_end_matches('}')
                .trim();
            let combined = if inner.is_empty() {
                signal_entries.join(", ")
            } else {
                format!("{inner}, {}", signal_entries.join(", "))
            };
            Some(format!("outputs: {{ {combined} }}"))
        }
    }
}

/// Map a host binding target to the Ivy runtime symbol it dispatches to.
/// Mirrors the dispatch logic in `host_codegen::dispatch_property_binding`.
fn host_instruction_for(target: &str) -> &'static str {
    if target.starts_with("style.") {
        "\u{0275}\u{0275}styleProp"
    } else if target == "class" {
        "\u{0275}\u{0275}classMap"
    } else if target.starts_with("class.") {
        "\u{0275}\u{0275}classProp"
    } else if target.starts_with("attr.") {
        "\u{0275}\u{0275}attribute"
    } else {
        "\u{0275}\u{0275}property"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{ConstructorParam, DecoratorCommon};

    fn make_directive(
        class_name: &str,
        selector: Option<&str>,
        standalone: bool,
    ) -> ExtractedDirective {
        ExtractedDirective {
            class_name: class_name.to_string(),
            selector: selector.map(|s| s.to_string()),
            standalone,
            inputs_source: None,
            outputs_source: None,
            export_as: None,
            constructor_params: Vec::new(),
            host_listeners: Vec::new(),
            host_bindings: Vec::new(),
            host_directives_source: None,
            signal_inputs: Vec::new(),
            signal_outputs: Vec::new(),
            signal_models: Vec::new(),
            signal_queries: Vec::new(),
            common: DecoratorCommon {
                decorator_span: (0, 0),
                class_body_start: 0,
                angular_core_import_span: None,
                other_angular_core_imports: Vec::new(),
            },
        }
    }

    #[test]
    fn test_directive_basic() {
        let extracted = make_directive("HighlightDirective", Some("[appHighlight]"), true);
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output.factory_code.contains("HighlightDirective_Factory"));
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}defineDirective"));
        assert!(output.static_fields[0].contains("type: HighlightDirective"));
        assert!(output.static_fields[0].contains("selectors: [['', 'appHighlight', '']]"));
        assert!(output.static_fields[0].contains("standalone: true"));
    }

    #[test]
    fn test_directive_element_selector() {
        let extracted = make_directive("MyComp", Some("my-comp"), false);
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output.static_fields[0].contains("selectors: [['my-comp']]"));
        assert!(!output.static_fields[0].contains("standalone"));
    }

    #[test]
    fn test_directive_no_selector() {
        let extracted = make_directive("AbstractDir", None, false);
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(!output.static_fields[0].contains("selectors"));
    }

    #[test]
    fn test_directive_with_deps() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.constructor_params = vec![ConstructorParam {
            type_name: Some("ElementRef".to_string()),
            inject_token: None,
            optional: false,
            self_: false,
            skip_self: false,
            host: false,
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output
            .factory_code
            .contains("\u{0275}\u{0275}inject(ElementRef)"));
    }

    #[test]
    fn test_directive_with_export_as() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.export_as = Some("myDir".to_string());
        let output = generate_directive_ivy(&extracted).unwrap();
        assert!(output.static_fields[0].contains("exportAs: [\"myDir\"]"));
    }

    /// Decorator-style host listener should produce the same `ɵɵlistener`
    /// statement shape as the linker's partial-declaration path.
    #[test]
    fn test_directive_host_listener() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_listeners = vec![HostListenerSpec {
            event: "click".to_string(),
            handler_expression: "onClick($event)".to_string(),
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(
            def.contains(
                "\u{0275}\u{0275}listener(\"click\", function($event) { return ctx.onClick($event); })"
            ),
            "expected ɵɵlistener call, got: {def}"
        );
        assert!(def.contains("if (rf & 1)"));
        // listeners do not contribute hostVars
        assert!(!def.contains("hostVars"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}listener"));
    }

    #[test]
    fn test_directive_host_binding_bare_property() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_bindings = vec![HostBindingSpec {
            target: "disabled".to_string(),
            property_name: "isDisabled".to_string(),
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(def.contains("\u{0275}\u{0275}property(\"disabled\", ctx.isDisabled)"));
        assert!(def.contains("hostVars: 1"));
        assert!(def.contains("if (rf & 2)"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}property"));
    }

    #[test]
    fn test_directive_host_binding_attr() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_bindings = vec![HostBindingSpec {
            target: "attr.aria-label".to_string(),
            property_name: "label".to_string(),
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(def.contains("\u{0275}\u{0275}attribute(\"aria-label\", ctx.label)"));
        assert!(def.contains("hostVars: 1"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}attribute"));
    }

    #[test]
    fn test_directive_host_binding_class_prop() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_bindings = vec![HostBindingSpec {
            target: "class.active".to_string(),
            property_name: "isActive".to_string(),
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(def.contains("\u{0275}\u{0275}classProp(\"active\", ctx.isActive)"));
        assert!(def.contains("hostVars: 2"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}classProp"));
    }

    #[test]
    fn test_directive_host_binding_style_simple() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_bindings = vec![HostBindingSpec {
            target: "style.color".to_string(),
            property_name: "color".to_string(),
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(def.contains("\u{0275}\u{0275}styleProp(\"color\", ctx.color)"));
        assert!(def.contains("hostVars: 2"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}styleProp"));
    }

    /// `style.width.px` must split off the unit suffix so the runtime gets
    /// `ɵɵstyleProp("width", value, "px")` — not `ɵɵstyleProp("width.px", value)`.
    #[test]
    fn test_directive_host_binding_style_with_unit() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_bindings = vec![HostBindingSpec {
            target: "style.width.px".to_string(),
            property_name: "width".to_string(),
        }];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(
            def.contains("\u{0275}\u{0275}styleProp(\"width\", ctx.width, \"px\")"),
            "expected ɵɵstyleProp with unit suffix, got: {def}"
        );
        assert!(def.contains("hostVars: 2"));
    }

    #[test]
    fn directive_host_directives_object_form_emits_feature() {
        // AOT path: a `@Directive` with `hostDirectives: [{ directive, inputs, outputs }]`
        // must wrap the array in `ɵɵHostDirectivesFeature(...)` inside the
        // emitted `features` array, normalise the decorator's colon-syntax
        // `inputs`/`outputs` into the runtime's flat-pair form, and add the
        // feature symbol to ivy_imports so the rewrite step pulls it in.
        let mut extracted = make_directive("HostDir", Some("[appHost]"), true);
        extracted.host_directives_source = Some(
            "[{ directive: ChildDir, inputs: ['childInput', 'aliased: localName'], outputs: ['childOutput'] }]"
                .to_string(),
        );
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(
            def.contains("features: [\u{0275}\u{0275}HostDirectivesFeature(["),
            "expected ɵɵHostDirectivesFeature in features array, got: {def}"
        );
        assert!(def.contains("directive: ChildDir"));
        assert!(
            def.contains("inputs: ['childInput', 'childInput', 'aliased', 'localName']"),
            "expected flat-pair inputs after colon-split: {def}"
        );
        assert!(
            def.contains("outputs: ['childOutput', 'childOutput']"),
            "expected bare output expanded to identity pair: {def}"
        );
        assert!(
            !def.contains("'aliased: localName'"),
            "raw colon-syntax string must not survive to the runtime: {def}"
        );
        assert!(output
            .ivy_imports
            .contains("\u{0275}\u{0275}HostDirectivesFeature"));
    }

    #[test]
    fn directive_host_directives_bare_form_emits_feature() {
        let mut extracted = make_directive("HostDir", Some("[appHost]"), true);
        extracted.host_directives_source = Some("[BareChild]".to_string());
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        assert!(
            def.contains("\u{0275}\u{0275}HostDirectivesFeature([BareChild])"),
            "expected bare class ref in feature call, got: {def}"
        );
    }

    #[test]
    fn test_directive_host_mixed_listener_and_bindings() {
        let mut extracted = make_directive("MyDir", Some("[myDir]"), true);
        extracted.host_listeners = vec![HostListenerSpec {
            event: "click".to_string(),
            handler_expression: "onClick($event)".to_string(),
        }];
        extracted.host_bindings = vec![
            HostBindingSpec {
                target: "attr.aria-label".to_string(),
                property_name: "label".to_string(),
            },
            HostBindingSpec {
                target: "class.active".to_string(),
                property_name: "isActive".to_string(),
            },
            HostBindingSpec {
                target: "disabled".to_string(),
                property_name: "isDisabled".to_string(),
            },
        ];
        let output = generate_directive_ivy(&extracted).unwrap();
        let def = &output.static_fields[0];
        // attr (1) + class (2) + property (1) = 4
        assert!(
            def.contains("hostVars: 4"),
            "expected hostVars: 4, got: {def}"
        );
        assert!(def.contains("\u{0275}\u{0275}listener(\"click\""));
        assert!(def.contains("\u{0275}\u{0275}attribute(\"aria-label\""));
        assert!(def.contains("\u{0275}\u{0275}classProp(\"active\""));
        assert!(def.contains("\u{0275}\u{0275}property(\"disabled\""));
        assert!(def.contains("if (rf & 1)"));
        assert!(def.contains("if (rf & 2)"));
    }
}
