use std::collections::{BTreeMap, BTreeSet};

use ngc_diagnostics::NgcResult;

use crate::ast::*;
use crate::extract::ExtractedComponent;

/// Generated Ivy output for any Angular decorator.
#[derive(Debug, Clone)]
pub struct IvyOutput {
    /// The `static ɵfac = ...` field code.
    pub factory_code: String,
    /// Static definition fields (e.g. ɵcmp, ɵprov, ɵdir, ɵpipe, ɵmod + ɵinj).
    pub static_fields: Vec<String>,
    /// Child template functions (for @if, @for, @switch blocks).
    pub child_template_functions: Vec<String>,
    /// Set of Ivy runtime symbols needed from `@angular/core`.
    pub ivy_imports: BTreeSet<String>,
    /// Static attribute arrays for the `consts` property of defineComponent.
    pub consts: Vec<String>,
}

/// A single level in the template scope hierarchy.
#[derive(Debug, Clone)]
enum ScopeEntry {
    /// An `@if`/`@else`/`@switch` embedded view — no local variables.
    Conditional,
    /// An `@for` embedded view — declares an item variable from `$implicit`.
    Repeater { item_name: String },
}

/// The DOM namespace under which an element (and its descendants) is created.
/// Ivy's runtime tracks a single global namespace flag that each
/// ɵɵnamespaceHTML/SVG/MathML call flips — once set, every subsequent
/// elementStart/element in the same template function inherits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Html,
    Svg,
    MathMl,
}

/// Internal codegen state.
struct IvyCodegen {
    component_name: String,
    slot_index: u32,
    var_count: u32,
    creation: Vec<String>,
    update: Vec<String>,
    child_templates: Vec<ChildTemplate>,
    ivy_imports: BTreeSet<String>,
    child_counter: u32,
    /// Collected static attribute arrays for the `consts` field.
    consts: Vec<String>,
    /// Active `@let` declarations: maps variable name to slot index.
    let_declarations: Vec<(String, u32)>,
    /// Local variable names that should NOT get `ctx.` prefix in expressions.
    local_vars: BTreeSet<String>,
    /// Scope stack tracking the template nesting hierarchy. Each entry records
    /// what kind of embedded view we're inside and any variables it declares.
    /// Used to generate correct `ɵɵnextContext()` calls and variable access.
    scope_stack: Vec<ScopeEntry>,
    /// Last slot index emitted in the update block (for computing `ɵɵadvance` deltas).
    /// `None` means no advance has been emitted yet (runtime starts at selectedIndex=-1).
    last_update_slot: Option<u32>,
    /// Separate counter for pipe binding offsets.  Pipe bindings must be packed
    /// contiguously after all sequential bindings in the LView binding array.
    /// This counter tracks the pipe-relative offset (0, 2, 5, …) which
    /// `rewrite_pipe_offsets` later shifts by the sequential binding count.
    pipe_var_offset: u32,
    /// Template reference variables declared in the current template scope.
    /// Maps `#refName` → LView slot that stores the resolved ref value.
    /// Binding expressions reading these names are rewritten to `ɵɵreference(slot)`.
    template_refs: BTreeMap<String, u32>,
    /// Namespace most recently emitted in the current template function's
    /// creation block. Compared against each element's target namespace to
    /// decide whether a ɵɵnamespaceHTML/SVG/MathML transition is needed.
    namespace_state: Namespace,
    /// Stack of "child namespace" for each element currently being walked.
    /// Top of stack = namespace that direct children should render in.
    /// Entering `<svg>` pushes Svg; `<math>` pushes MathMl; `<foreignObject>`
    /// (which is itself SVG) pushes Html so its HTML descendants render correctly.
    namespace_stack: Vec<Namespace>,
    /// `true` once a `<ng-content>` is encountered. Triggers two
    /// out-of-band emissions on the host component:
    ///   1. `ɵɵprojectionDef()` at the head of the create block — the
    ///      runtime walks projected nodes through `tNode.projection`
    ///      which `ɵɵprojection(idx)` later reads. Without the def
    ///      call that field stays null and `ɵɵprojection` blows up
    ///      with `Cannot read properties of null (reading '0')`.
    ///   2. `ngContentSelectors: ['*']` on the `defineComponent` call
    ///      so Angular's runtime knows what selectors the host
    ///      projects. Default-projection components only need `['*']`;
    ///      multi-slot projection (`<ng-content select="...">`) is
    ///      not yet implemented.
    has_projection: bool,
}

struct ChildTemplate {
    #[allow(dead_code)]
    function_name: String,
    decls: u32,
    vars: u32,
    code: String,
}

/// Generate Ivy instructions from a parsed template AST and component metadata.
pub fn generate_ivy(
    component: &ExtractedComponent,
    template_nodes: &[TemplateNode],
) -> NgcResult<IvyOutput> {
    let mut gen = IvyCodegen {
        component_name: component.class_name.clone(),
        slot_index: 0,
        var_count: 0,
        creation: Vec::new(),
        update: Vec::new(),
        child_templates: Vec::new(),
        ivy_imports: BTreeSet::new(),
        child_counter: 0,
        consts: Vec::new(),
        let_declarations: Vec::new(),
        local_vars: BTreeSet::new(),
        scope_stack: Vec::new(),
        last_update_slot: None,
        pipe_var_offset: 0,
        template_refs: BTreeMap::new(),
        namespace_state: Namespace::Html,
        namespace_stack: vec![Namespace::Html],
        has_projection: false,
    };

    gen.ivy_imports
        .insert("\u{0275}\u{0275}defineComponent".to_string());

    gen.generate_nodes(template_nodes);

    let decls = gen.slot_index;
    // Count sequential bindings (non-pipe) vs pipe bindings to fix offsets
    let seq_bindings = count_sequential_bindings(&gen.update);
    let pipe_binding_total = count_pipe_binding_slots(&gen.update);
    let vars = seq_bindings + pipe_binding_total;

    // Build template function body
    let mut template_body = String::new();
    if !gen.creation.is_empty() {
        template_body.push_str("    if (rf & 1) {\n");
        // When the template uses `<ng-content>`, the runtime needs
        // `ɵɵprojectionDef()` to run once at the head of the create
        // block so it can stash the projected children's TNodes onto
        // the host's TNode. Subsequent `ɵɵprojection(idx)` calls then
        // read `tNode.projection[idx]` — without the def call that's
        // null and the runtime throws on the first projection.
        if gen.has_projection {
            gen.ivy_imports
                .insert("\u{0275}\u{0275}projectionDef".to_string());
            template_body.push_str("      \u{0275}\u{0275}projectionDef();\n");
        }
        for instr in &gen.creation {
            template_body.push_str("      ");
            template_body.push_str(instr);
            template_body.push('\n');
        }
        template_body.push_str("    }\n");
    }
    if !gen.update.is_empty() {
        template_body.push_str("    if (rf & 2) {\n");
        // Root template: `ctx` is the function parameter, so refs can be
        // read anywhere — but emit them at the top so bindings below can
        // use the bare identifier names and not refer to ɵɵreference().
        let ref_prelude = gen.build_ref_reads_prelude("      ");
        template_body.push_str(&ref_prelude);
        for instr in &gen.update {
            template_body.push_str("      ");
            let rewritten = rewrite_pipe_offsets(instr, seq_bindings);
            template_body.push_str(&rewritten);
            template_body.push('\n');
        }
        template_body.push_str("    }\n");
    }

    // Build factory
    let factory_code = format!(
        "static \u{0275}fac = function {name}_Factory(t: any) {{ return new (t || {name})(); }}",
        name = component.class_name
    );

    // Build defineComponent
    let mut dc = String::new();
    dc.push_str("static \u{0275}cmp = \u{0275}\u{0275}defineComponent({\n");
    dc.push_str(&format!("    type: {},\n", component.class_name));
    dc.push_str(&format!("    selectors: [['{}']],\n", component.selector));
    if component.standalone {
        dc.push_str("    standalone: true,\n");
    }
    // Merge `@Input()` decorator-style fields and signal-API class-field
    // initialisations (`input()`, `output()`, `model()`, view/content
    // queries) into a single `inputs` / `outputs` map plus optional
    // `viewQuery` / `contentQueries` functions. Signal-based inputs
    // ride along with the `SignalBased` runtime flag so the directive
    // runtime knows to write into the WritableSignal rather than a
    // plain field.
    let signal_members = crate::signal_codegen::compile_signal_members(
        &component.input_properties,
        &component.signal_inputs,
        &component.signal_outputs,
        &component.signal_models,
        &component.signal_queries,
    );
    for sym in &signal_members.ivy_imports {
        gen.ivy_imports.insert(sym.clone());
    }
    if let Some(inputs) =
        crate::signal_codegen::format_map("inputs", &signal_members.inputs_entries)
    {
        dc.push_str(&format!("    {inputs},\n"));
    }
    if let Some(outputs) =
        crate::signal_codegen::format_map("outputs", &signal_members.outputs_entries)
    {
        dc.push_str(&format!("    {outputs},\n"));
    }
    let host_props = crate::directive_codegen::build_host_props(
        &component.host_listeners,
        &component.host_bindings,
    );
    for imp in &host_props.ivy_imports {
        gen.ivy_imports.insert(imp.clone());
    }
    if let Some(ref host_vars) = host_props.host_vars_prop {
        dc.push_str(&format!("    {host_vars},\n"));
    }
    if let Some(ref host_bindings) = host_props.host_bindings_prop {
        dc.push_str(&format!("    {host_bindings},\n"));
    }
    // hostDirectives composition (Angular 15+). Wrap the array in
    // `ɵɵHostDirectivesFeature(...)` and emit as a `features` entry so the
    // runtime instantiates the composed directives on the host element. The
    // array is normalised first: decorator-form `inputs`/`outputs` use
    // `'public: private'` colon syntax, but the runtime expects flat-pair
    // arrays (`bindings[i]`, `bindings[i+1]`).
    if let Some(ref host_dirs_src) = component.host_directives_source {
        gen.ivy_imports
            .insert("\u{0275}\u{0275}HostDirectivesFeature".to_string());
        let normalised = crate::host_codegen::transform_host_directives_array(host_dirs_src)
            .unwrap_or_else(|| host_dirs_src.clone());
        dc.push_str(&format!(
            "    features: [\u{0275}\u{0275}HostDirectivesFeature({normalised})],\n"
        ));
    }
    // Components that project content need `ngContentSelectors` on the
    // def so Angular's runtime can match `<ng-content select="...">`
    // against the host's projected children. We only emit `<ng-content>`
    // (no select-attribute support yet), so a single-entry `['*']`
    // (catch-all) selector list is sufficient. The matching
    // `ɵɵprojectionDef()` call lives at the head of the create block.
    if gen.has_projection {
        dc.push_str("    ngContentSelectors: [\"*\"],\n");
    }
    if !gen.consts.is_empty() {
        dc.push_str(&format!("    consts: [{}],\n", gen.consts.join(", ")));
    }
    dc.push_str(&format!("    decls: {decls},\n"));
    dc.push_str(&format!("    vars: {vars},\n"));
    dc.push_str(&format!(
        "    template: function {}_Template(rf: number, ctx: {}) {{\n",
        component.class_name, component.class_name
    ));
    dc.push_str(&template_body);
    dc.push_str("    }");

    // Signal-based queries are emitted as their own `viewQuery` /
    // `contentQueries` functions on the define call (signal queries
    // and decorator-style queries can't share a function — the signal
    // variant uses `ɵɵqueryAdvance`, the decorator variant uses
    // `ɵɵqueryRefresh`/`ɵɵloadQuery`). When this component eventually
    // supports decorator-style `@ViewChild` queries on the AOT path
    // too, those will need to be merged in here as additional
    // statements within the same function body.
    if let Some(ref vq) = signal_members.view_query_prop {
        dc.push_str(&format!(",\n    {vq}"));
    }
    if let Some(ref cq) = signal_members.content_queries_prop {
        dc.push_str(&format!(",\n    {cq}"));
    }

    // Component dependencies. Emit the imports array verbatim here; the linker's
    // post-pass (crates/linker/src/module_registry.rs) walks every
    // ɵɵdefineComponent and flattens any NgModule identifiers in this array to
    // their transitively-exported directive/pipe classes — mirroring ng build.
    if let Some(ref imports_src) = component.imports_source {
        dc.push_str(&format!(",\n    dependencies: {imports_src}"));
    }
    if let Some(ref styles_src) = component.styles_source {
        // Pre-scope CSS with %COMP% placeholders for Angular's emulated ViewEncapsulation.
        // The runtime replaces %COMP% with the component's unique ID.
        let scoped = scope_component_styles(styles_src);
        dc.push_str(&format!(",\n    styles: {scoped}"));
    }
    if let Some(ref animations_src) = component.animations_source {
        // Angular's runtime reads `data.animation` when resolving `@`-prefixed
        // property/listener bindings through the animation renderer.
        dc.push_str(&format!(",\n    data: {{ animation: {animations_src} }}"));
    }
    if let Some(cd) = component.change_detection {
        // `changeDetection: 0` (`OnPush`) is what flips
        // `def.onPush = true`, which in turn makes the runtime use
        // OnPush-style refresh — the only mode that respects signal-driven
        // change detection in zoneless apps. Without it, components
        // running under `provideZonelessChangeDetection()` never re-render
        // on signal writes and event-handler updates appear inert.
        dc.push_str(&format!(",\n    changeDetection: {cd}"));
    }
    dc.push_str("\n  })");

    // Collect child template functions
    let child_fns: Vec<String> = gen
        .child_templates
        .iter()
        .map(|ct| ct.code.clone())
        .collect();

    Ok(IvyOutput {
        factory_code,
        static_fields: vec![dc],
        child_template_functions: child_fns,
        ivy_imports: gen.ivy_imports,
        consts: gen.consts,
    })
}

impl IvyCodegen {
    fn generate_nodes(&mut self, nodes: &[TemplateNode]) {
        let mut i = 0;
        while i < nodes.len() {
            // Merge [Interpolation, Text] into textInterpolate1 when the text
            // is a short inline suffix within the same parent element.
            // This matches Angular's ngtsc which produces
            // `ɵɵtextInterpolate1("", pipedValue, " @")` instead of
            // separate text + textInterpolate instructions.
            if let TemplateNode::Interpolation(interp) = &nodes[i] {
                if i + 1 < nodes.len() {
                    if let TemplateNode::Text(t) = &nodes[i + 1] {
                        let trimmed = t.value.trim();
                        // Merge if: non-empty suffix, short, no block-level content,
                        // and the next node after the text is NOT another interpolation
                        // (which would need its own slot).
                        let is_last_or_followed_by_element = i + 2 >= nodes.len()
                            || matches!(&nodes[i + 2], TemplateNode::Element(_));
                        if !trimmed.is_empty()
                            && trimmed.len() < 30
                            && !trimmed.contains('<')
                            && is_last_or_followed_by_element
                        {
                            let suffix = t.value.replace('\n', " ");
                            let suffix = suffix.trim();
                            self.generate_interpolation_with_suffix(interp, suffix);
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            self.generate_node(&nodes[i]);
            i += 1;
        }
    }

    fn generate_node(&mut self, node: &TemplateNode) {
        match node {
            TemplateNode::Element(el) => self.generate_element(el),
            TemplateNode::Text(text) => self.generate_text(text),
            TemplateNode::Interpolation(interp) => self.generate_interpolation(interp),
            TemplateNode::IfBlock(block) => self.generate_if_block(block),
            TemplateNode::ForBlock(block) => self.generate_for_block(block),
            TemplateNode::SwitchBlock(block) => self.generate_switch_block(block),
            TemplateNode::LetDeclaration(decl) => self.generate_let_declaration(decl),
            TemplateNode::DeferBlock(block) => self.generate_defer_block(block),
            TemplateNode::IcuExpression(icu) => self.generate_icu_expression(icu),
        }
    }

    /// Emit an ICU message as a `$localize`-tagged placeholder text node.
    ///
    /// Full ICU codegen (`ɵɵi18n` + runtime selector arms) is substantial;
    /// this first pass renders the `other` branch (or the first case when
    /// `other` is missing) so templates parse and render, while the
    /// extractor still sees the complete message for translation.
    fn generate_icu_expression(&mut self, icu: &crate::ast::IcuExpressionNode) {
        let fallback = icu
            .cases
            .iter()
            .find(|c| c.key == "other")
            .or_else(|| icu.cases.first())
            .map(|c| c.body.clone())
            .unwrap_or_default();
        let slot = self.slot_index;
        self.slot_index += 1;
        let escaped = escape_js_string(&fallback);
        self.creation.push(format!(
            "        \u{0275}\u{0275}text({slot}, '{escaped}');"
        ));
    }

    /// Namespace the current position's elements inherit — top of the stack,
    /// or Html if the stack is empty (should not happen in practice).
    fn current_namespace(&self) -> Namespace {
        self.namespace_stack
            .last()
            .copied()
            .unwrap_or(Namespace::Html)
    }

    /// Namespace an element with the given tag should render in, given the
    /// inherited namespace. `svg` and `math` force their own namespaces
    /// regardless of parent; every other tag inherits.
    fn element_namespace(tag: &str, inherited: Namespace) -> Namespace {
        match tag {
            "svg" => Namespace::Svg,
            "math" => Namespace::MathMl,
            _ => inherited,
        }
    }

    /// Namespace the children of an element with the given tag should render
    /// in. `<foreignObject>` is itself an SVG element but its descendants
    /// return to HTML until the subtree ends.
    fn child_namespace(tag: &str, element_ns: Namespace) -> Namespace {
        match (tag, element_ns) {
            ("svg", _) => Namespace::Svg,
            ("math", _) => Namespace::MathMl,
            // foreignObject inside SVG hands control back to HTML for its
            // descendants. Case-sensitive match mirrors Angular / the HTML5
            // parser behavior.
            ("foreignObject", Namespace::Svg) => Namespace::Html,
            (_, ns) => ns,
        }
    }

    /// Emit a `ɵɵnamespaceHTML/SVG/MathML()` transition to the creation block
    /// if the given target differs from `namespace_state`; update the state.
    fn emit_namespace_transition(&mut self, target: Namespace) {
        if self.namespace_state == target {
            return;
        }
        let instr = match target {
            Namespace::Html => "\u{0275}\u{0275}namespaceHTML",
            Namespace::Svg => "\u{0275}\u{0275}namespaceSVG",
            Namespace::MathMl => "\u{0275}\u{0275}namespaceMathML",
        };
        self.ivy_imports.insert(instr.to_string());
        self.creation.push(format!("{instr}();"));
        self.namespace_state = target;
    }

    fn generate_element(&mut self, el: &ElementNode) {
        // Check for structural directive — desugar to ng-template wrapper
        let structural = el.attributes.iter().find_map(|a| match a {
            TemplateAttribute::StructuralDirective { name, expression } => {
                Some((name.clone(), expression.clone()))
            }
            _ => None,
        });
        if let Some((dir_name, dir_expr)) = structural {
            self.generate_structural_directive(el, &dir_name, &dir_expr);
            return;
        }

        // Elements carrying an `i18n` attribute take the i18n code path:
        // children are absorbed into a `$localize` message, and the
        // standard creation/update instructions are replaced with
        // `ɵɵi18n` / `ɵɵi18nExp` / `ɵɵi18nApply`.
        let i18n_meta = el.attributes.iter().find_map(|a| match a {
            TemplateAttribute::I18n(meta) => Some(meta.clone()),
            _ => None,
        });
        if let Some(meta) = i18n_meta {
            self.generate_i18n_element(el, &meta);
            return;
        }

        // Special Angular elements
        match el.tag.as_str() {
            "ng-content" => {
                // `ɵɵprojection(idx)` reads `tNode.projection` to find
                // the projected children for slot `idx`. That field is
                // populated by `ɵɵprojectionDef()`, which has to run
                // *once* at the head of the host component's create
                // block — see `has_projection` in the codegen state.
                let slot = self.slot_index;
                self.slot_index += 1;
                self.has_projection = true;
                self.ivy_imports
                    .insert("\u{0275}\u{0275}projection".to_string());
                self.creation
                    .push(format!("\u{0275}\u{0275}projection({slot});"));
                return;
            }
            "ng-template" => {
                let slot = self.slot_index;
                self.slot_index += 1;
                self.ivy_imports
                    .insert("\u{0275}\u{0275}template".to_string());
                let fn_name = format!(
                    "{}_NgTemplate_{}_Template",
                    self.component_name, self.child_counter
                );
                self.child_counter += 1;
                let child = self.generate_child_template(&fn_name, &el.children);
                // Collect static attributes for the ng-template (needed for directive
                // matching, e.g. `<ng-template cdkPortalOutlet />`).
                let tpl_attrs: Vec<(&str, &str)> = el
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        TemplateAttribute::Static {
                            name,
                            value: Some(v),
                        } => Some((name.as_str(), v.as_str())),
                        TemplateAttribute::Static { name, value: None } => {
                            Some((name.as_str(), ""))
                        }
                        _ => None,
                    })
                    .collect();
                let tpl_bindings: Vec<&str> = el
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        TemplateAttribute::Property { name, .. } => Some(name.as_str()),
                        _ => None,
                    })
                    .collect();
                if tpl_attrs.is_empty() && tpl_bindings.is_empty() {
                    self.creation.push(format!(
                        "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {});",
                        child.decls, child.vars
                    ));
                } else {
                    let ci = self.register_const_with_bindings(&tpl_attrs, &tpl_bindings);
                    self.creation.push(format!(
                        "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {}, 'ng-template', {ci});",
                        child.decls, child.vars
                    ));
                }
                self.child_templates.push(child);
                return;
            }
            _ => {}
        }

        let slot = self.slot_index;
        self.slot_index += 1;

        let is_ng_container = el.tag == "ng-container";

        // Check for event bindings — void elements with listeners need
        // elementStart/elementEnd so ɵɵlistener() calls can be placed between them.
        let has_events = el.attributes.iter().any(|a| {
            matches!(
                a,
                TemplateAttribute::Event { .. } | TemplateAttribute::TwoWayBinding { .. }
            )
        });

        // Static attributes for consts — include value-less (boolean/directive)
        // attributes with an empty string so that directive selectors like
        // `canvas[baseChart]` can match at runtime.
        let static_attrs: Vec<(&str, &str)> = el
            .attributes
            .iter()
            .filter_map(|a| match a {
                TemplateAttribute::Static {
                    name,
                    value: Some(v),
                } => Some((name.as_str(), v.as_str())),
                TemplateAttribute::Static { name, value: None } => Some((name.as_str(), "")),
                _ => None,
            })
            .collect();

        // Collect property binding names for the consts array.
        // Angular uses AttributeMarker.Bindings (3) to mark binding names so that
        // directive matching can find directives by their input selectors.
        // Only include Property and TwoWayBinding — NOT ClassBinding ([class.x]),
        // StyleBinding ([style.x]), or AttrBinding ([attr.x]) since those are handled
        // by built-in instructions and don't participate in directive matching.
        // Also exclude [class] and [style] which map to classMap/styleMap.
        // Angular emits event output names BEFORE input property names in the
        // binding markers section.  Collect them in two passes to match ng build order.
        let mut binding_names: Vec<&str> = Vec::new();
        // Pass 1: event outputs
        for a in &el.attributes {
            if let TemplateAttribute::Event { name, .. } = a {
                binding_names.push(name.as_str());
            }
        }
        // Pass 2: property inputs and two-way bindings
        for a in &el.attributes {
            match a {
                TemplateAttribute::Property { name, .. }
                    if name != "class"
                        && name != "style"
                        && !name.starts_with("attr.")
                        && !name.starts_with("style.")
                        && !name.starts_with("class.") =>
                {
                    binding_names.push(name.as_str());
                }
                TemplateAttribute::TwoWayBinding { name, .. } => {
                    binding_names.push(name.as_str());
                }
                _ => {}
            }
        }

        // Check if we need a consts entry (static attrs OR binding markers)
        let has_consts = !static_attrs.is_empty() || !binding_names.is_empty();
        let const_idx = if has_consts {
            Some(self.register_const_with_bindings(&static_attrs, &binding_names))
        } else {
            None
        };

        // Collect template reference variables on this element (e.g.
        // `#profileForm="ngForm"`, `#fileInput`).  Register a consts entry
        // `[name, exportAs]` per ref — Angular's runtime populates the ref
        // value into the LView slot immediately following the element.
        let refs: Vec<(String, String)> = el
            .attributes
            .iter()
            .filter_map(|a| match a {
                TemplateAttribute::Reference { name, export_as } => {
                    Some((name.clone(), export_as.clone().unwrap_or_default()))
                }
                _ => None,
            })
            .collect();
        let refs_const_idx = if refs.is_empty() {
            None
        } else {
            Some(self.register_refs_const(&refs))
        };

        // Resolve this element's DOM namespace (svg/math force their own;
        // others inherit from the enclosing subtree) and emit a runtime
        // ɵɵnamespace* transition if it differs from the current state.
        // ng-container is a virtual wrapper and doesn't itself take a
        // namespace, but its children still inherit the outer context.
        let inherited_ns = self.current_namespace();
        let element_ns = Self::element_namespace(&el.tag, inherited_ns);
        if !is_ng_container {
            self.emit_namespace_transition(element_ns);
        }

        if el.is_void && !is_ng_container && !has_events {
            let instr = "\u{0275}\u{0275}element";
            self.ivy_imports.insert(instr.to_string());
            match (const_idx, refs_const_idx) {
                (Some(ci), Some(ri)) => self
                    .creation
                    .push(format!("{instr}({slot}, '{}', {ci}, {ri});", el.tag)),
                (Some(ci), None) => self
                    .creation
                    .push(format!("{instr}({slot}, '{}', {ci});", el.tag)),
                (None, Some(ri)) => self
                    .creation
                    .push(format!("{instr}({slot}, '{}', null, {ri});", el.tag)),
                (None, None) => self
                    .creation
                    .push(format!("{instr}({slot}, '{}');", el.tag)),
            }

            // Allocate ref slots BEFORE emitting bindings so that expressions
            // using the ref name can resolve to the correct ɵɵreference(slot).
            self.reserve_template_refs(&refs);

            self.emit_i18n_attributes(el, slot);

            // Bindings for void elements
            self.emit_element_bindings(el, slot);

            // Emit ɵɵreference(slot) calls to register the refs at their slots.
            self.emit_reference_calls(&refs);
        } else {
            let (start_instr, end_instr) = if is_ng_container {
                (
                    "\u{0275}\u{0275}elementContainerStart",
                    "\u{0275}\u{0275}elementContainerEnd",
                )
            } else {
                ("\u{0275}\u{0275}elementStart", "\u{0275}\u{0275}elementEnd")
            };
            self.ivy_imports.insert(start_instr.to_string());
            self.ivy_imports.insert(end_instr.to_string());
            if is_ng_container {
                match (const_idx, refs_const_idx) {
                    (Some(ci), Some(ri)) => self
                        .creation
                        .push(format!("{start_instr}({slot}, {ci}, {ri});")),
                    (Some(ci), None) => self.creation.push(format!("{start_instr}({slot}, {ci});")),
                    (None, Some(ri)) => self
                        .creation
                        .push(format!("{start_instr}({slot}, null, {ri});")),
                    (None, None) => self.creation.push(format!("{start_instr}({slot});")),
                }
            } else {
                match (const_idx, refs_const_idx) {
                    (Some(ci), Some(ri)) => self
                        .creation
                        .push(format!("{start_instr}({slot}, '{}', {ci}, {ri});", el.tag)),
                    (Some(ci), None) => self
                        .creation
                        .push(format!("{start_instr}({slot}, '{}', {ci});", el.tag)),
                    (None, Some(ri)) => self
                        .creation
                        .push(format!("{start_instr}({slot}, '{}', null, {ri});", el.tag)),
                    (None, None) => self
                        .creation
                        .push(format!("{start_instr}({slot}, '{}');", el.tag)),
                }
            }

            // Allocate ref slots BEFORE listeners, bindings, or children.
            // Listener bodies and descendant bindings may reference these names.
            self.reserve_template_refs(&refs);

            // Event listeners and two-way binding listeners in creation block
            for attr in &el.attributes {
                match attr {
                    TemplateAttribute::Event { name, handler } => {
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}listener".to_string());
                        let compiled_handler = self.compile_listener_handler(handler);
                        let depth = self.scope_depth();
                        if depth > 0 {
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}restoreView".to_string());
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}nextContext".to_string());
                            let listener_preamble = self.generate_listener_preamble();
                            self.creation.push(format!(
                                "\u{0275}\u{0275}listener('{}', function($event) {{ {listener_preamble}{compiled_handler} }});",
                                name,
                            ));
                        } else {
                            self.creation.push(format!(
                                "\u{0275}\u{0275}listener('{}', function($event) {{ {compiled_handler} }});",
                                name,
                            ));
                        }
                    }
                    TemplateAttribute::TwoWayBinding { name, expression } => {
                        // Two-way listener `(xChange)="expr = $event"`
                        // for signal-aware bindings: the runtime helper
                        // `ɵɵtwoWayBindingSet(target, value)` calls
                        // `.set(value)` on a `WritableSignal` and
                        // returns `true`; for non-signals it returns
                        // `false`, so the `||` falls through to a
                        // plain assignment. Writing `target = $event`
                        // unconditionally would replace the signal
                        // FIELD on the parent (turning
                        // `parentActive: WritableSignal<boolean>` into
                        // `parentActive: boolean`), and the very next
                        // `parentActive()` template read would throw
                        // "is not a function".
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}twoWayListener".to_string());
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}twoWayBindingSet".to_string());
                        let depth = self.scope_depth();
                        let compiled_target = ctx_expr_with_locals(expression, &self.local_vars);
                        let body = format!(
                            "\u{0275}\u{0275}twoWayBindingSet({compiled_target}, $event) || ({compiled_target} = $event); return $event;"
                        );
                        if depth > 0 {
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}restoreView".to_string());
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}nextContext".to_string());
                            let listener_preamble = self.generate_listener_preamble();
                            self.creation.push(format!(
                                "\u{0275}\u{0275}twoWayListener('{}Change', function($event) {{ {listener_preamble}{body} }});",
                                name,
                            ));
                        } else {
                            self.creation.push(format!(
                                "\u{0275}\u{0275}twoWayListener('{}Change', function($event) {{ {body} }});",
                                name,
                            ));
                        }
                    }
                    _ => {}
                }
            }

            self.emit_i18n_attributes(el, slot);

            // Property bindings in update block — emitted before children
            // so ɵɵadvance() targets the correct element slot.
            self.emit_element_bindings(el, slot);

            // Push the namespace that direct children render in.
            // `<foreignObject>` inside SVG resets to HTML for the subtree,
            // restored automatically when we pop after children.
            let child_ns = Self::child_namespace(&el.tag, element_ns);
            self.namespace_stack.push(child_ns);
            self.generate_nodes(&el.children);
            self.namespace_stack.pop();

            self.creation.push(format!("{end_instr}();"));

            // Emit ɵɵreference(slot) calls after elementEnd to register the
            // refs at their pre-reserved slots (matches Angular's output).
            self.emit_reference_calls(&refs);
        }
    }

    /// Compile an element whose `i18n` attribute marks its children as a
    /// translatable message. The element itself is emitted normally
    /// (including any static attributes, property bindings, and
    /// `i18n-<attr>` markers), then its children are replaced with an
    /// `ɵɵi18n(slot, msgIdx)` creation instruction plus the matching
    /// `ɵɵi18nExp` / `ɵɵi18nApply` update-block calls for each
    /// interpolation placeholder in the message.
    fn generate_i18n_element(&mut self, el: &ElementNode, meta: &I18nMeta) {
        let message = crate::i18n::compile_message(&el.children, meta);
        // Strip the `i18n` marker from attributes before emitting — it's
        // metadata for the compiler, not a runtime attribute.
        let attributes: Vec<TemplateAttribute> = el
            .attributes
            .iter()
            .filter(|a| !matches!(a, TemplateAttribute::I18n(_)))
            .cloned()
            .collect();

        // `ɵɵelementStart` takes the parent slot; the i18n block lives on
        // the next slot inside the element.
        let parent_slot = self.slot_index;
        self.slot_index += 1;
        let i18n_slot = self.slot_index;
        self.slot_index += 1;

        let msg_idx = self.register_i18n_const(&message.localize_expr);

        // Static attributes / binding markers for the enclosing element.
        let static_attrs: Vec<(&str, &str)> = attributes
            .iter()
            .filter_map(|a| match a {
                TemplateAttribute::Static {
                    name,
                    value: Some(v),
                } => Some((name.as_str(), v.as_str())),
                TemplateAttribute::Static { name, value: None } => Some((name.as_str(), "")),
                _ => None,
            })
            .collect();
        let consts_idx = if static_attrs.is_empty() {
            None
        } else {
            Some(self.register_const(&static_attrs))
        };

        let start_instr = "\u{0275}\u{0275}elementStart";
        let end_instr = "\u{0275}\u{0275}elementEnd";
        let i18n_instr = "\u{0275}\u{0275}i18n";
        self.ivy_imports.insert(start_instr.to_string());
        self.ivy_imports.insert(end_instr.to_string());
        self.ivy_imports.insert(i18n_instr.to_string());

        match consts_idx {
            Some(ci) => self
                .creation
                .push(format!("{start_instr}({parent_slot}, '{}', {ci});", el.tag)),
            None => self
                .creation
                .push(format!("{start_instr}({parent_slot}, '{}');", el.tag)),
        }
        self.creation
            .push(format!("{i18n_instr}({i18n_slot}, {msg_idx});"));
        self.creation.push(format!("{end_instr}();"));

        // Update block: one ɵɵi18nExp per placeholder, followed by a
        // single ɵɵi18nApply tying them to the i18n slot.
        if !message.interpolations.is_empty() {
            self.ivy_imports
                .insert("\u{0275}\u{0275}i18nExp".to_string());
            self.ivy_imports
                .insert("\u{0275}\u{0275}i18nApply".to_string());
            self.add_advance(i18n_slot);
            for expr in &message.interpolations {
                let compiled = ctx_expr_with_locals(expr, &self.local_vars);
                self.update
                    .push(format!("\u{0275}\u{0275}i18nExp({compiled});"));
                self.var_count += 1;
            }
            self.update
                .push(format!("\u{0275}\u{0275}i18nApply({i18n_slot});"));
        }
    }

    /// Register a bare expression (e.g. a `$localize`-tagged template
    /// literal) in the consts array and return its index. Unlike
    /// `register_const`, the payload is inserted verbatim rather than
    /// formatted as a static-attribute tuple array.
    fn register_i18n_const(&mut self, expr: &str) -> usize {
        let idx = self.consts.len();
        self.consts.push(expr.to_string());
        idx
    }

    /// Emit `ɵɵi18nAttributes(slot, idx)` for any `i18n-<attr>` markers on
    /// the element. Each marker is paired with a matching static attribute
    /// (by name); the attribute's value is compiled into a `$localize`
    /// message and referenced from a consts array that lists the affected
    /// attribute names under `AttributeMarker.I18n` (6).
    fn emit_i18n_attributes(&mut self, el: &ElementNode, slot: u32) {
        let mut markers: Vec<(String, I18nMeta)> = Vec::new();
        for a in &el.attributes {
            if let TemplateAttribute::I18nAttr { target, meta } = a {
                markers.push((target.clone(), meta.clone()));
            }
        }
        if markers.is_empty() {
            return;
        }

        // Build: [AttributeMarker.I18n (6), <name>, <messageConstIdx>, ...]
        // Each (name, msgIdx) pair follows the marker so the runtime knows
        // which attribute receives which translated message.
        let mut parts: Vec<String> = vec!["6".to_string()];
        for (target, meta) in &markers {
            let value = el
                .attributes
                .iter()
                .find_map(|a| match a {
                    TemplateAttribute::Static { name, value } if name == target => {
                        Some(value.clone().unwrap_or_default())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let msg = crate::i18n::compile_attribute_message(&value, meta);
            let msg_idx = self.register_i18n_const(&msg.localize_expr);
            parts.push(format!("'{}'", escape_js_string(target)));
            parts.push(msg_idx.to_string());
        }
        let marker_const = format!("[{}]", parts.join(", "));
        let marker_idx = self.consts.len();
        self.consts.push(marker_const);

        self.ivy_imports
            .insert("\u{0275}\u{0275}i18nAttributes".to_string());
        self.creation.push(format!(
            "\u{0275}\u{0275}i18nAttributes({slot}, {marker_idx});"
        ));
    }

    /// Reserve a slot for each template reference on the current element and
    /// register the ref in `template_refs` + `local_vars` so binding
    /// expressions can resolve it.
    fn reserve_template_refs(&mut self, refs: &[(String, String)]) {
        if refs.is_empty() {
            return;
        }
        self.ivy_imports
            .insert("\u{0275}\u{0275}reference".to_string());
        for (name, _) in refs {
            let ref_slot = self.slot_index;
            self.slot_index += 1;
            self.template_refs.insert(name.clone(), ref_slot);
            self.local_vars.insert(name.clone());
        }
    }

    /// No-op: ref slots are auto-allocated by Angular's runtime based on the
    /// `localRefsIndex` arg passed to `ɵɵelementStart`.  Emitting an explicit
    /// creation-mode `ɵɵreference(slot);` call causes double-registration and
    /// trips the Angular `assertIndexInRange` check at runtime.
    fn emit_reference_calls(&mut self, _refs: &[(String, String)]) {}

    /// Compile an event-handler expression with current locals.
    /// Template references inside the handler stay as bare identifiers;
    /// they resolve through the `const <name> = ɵɵreference(<slot>);`
    /// declarations injected at the top of the listener body by
    /// `build_ref_reads_prelude()`.
    fn compile_listener_handler(&mut self, handler: &str) -> String {
        compile_event_handler(handler, &self.local_vars)
    }

    /// Build the `const <name> = ɵɵreference(<slot>);` prelude that must be
    /// emitted BEFORE any `ɵɵnextContext()` call (the latter changes the
    /// context LView, so a subsequent `ɵɵreference()` would read from the
    /// wrong LView).  Returns an empty string when there are no refs in
    /// the current scope.
    fn build_ref_reads_prelude(&mut self, indent: &str) -> String {
        if self.template_refs.is_empty() {
            return String::new();
        }
        self.ivy_imports
            .insert("\u{0275}\u{0275}reference".to_string());
        let mut code = String::new();
        for (name, slot) in &self.template_refs {
            code.push_str(&format!(
                "{indent}const {name} = \u{0275}\u{0275}reference({slot});\n"
            ));
        }
        code
    }

    /// Desugar a structural directive (*ngIf, *ngFor) to an ng-template wrapper.
    fn generate_structural_directive(&mut self, el: &ElementNode, dir_name: &str, dir_expr: &str) {
        let slot = self.slot_index;
        self.slot_index += 1;

        self.ivy_imports
            .insert("\u{0275}\u{0275}template".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}property".to_string());

        // Create a child element without the structural directive
        let filtered_attrs: Vec<TemplateAttribute> = el
            .attributes
            .iter()
            .filter(|a| !matches!(a, TemplateAttribute::StructuralDirective { .. }))
            .cloned()
            .collect();
        let inner_el = ElementNode {
            tag: el.tag.clone(),
            attributes: filtered_attrs,
            children: el.children.clone(),
            is_void: el.is_void,
        };

        let fn_name = format!(
            "{}_Directive_{}_Template",
            self.component_name, self.child_counter
        );
        self.child_counter += 1;

        // Generate the child template containing the original element
        let child = self.generate_child_template_with_element(&fn_name, &inner_el);
        self.creation.push(format!(
            "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {});",
            child.decls, child.vars
        ));
        self.child_templates.push(child);

        // Property binding for the directive
        self.add_advance(slot);

        // Parse *ngFor micro-syntax: "let item of items" → property ngForOf
        if dir_name == "ngFor" {
            let binding_name = "ngForOf";
            if let Some(of_pos) = dir_expr.find(" of ") {
                let iterable = dir_expr[of_pos + 4..].trim();
                self.update.push(format!(
                    "\u{0275}\u{0275}property('{}', {});",
                    binding_name,
                    ctx_expr(iterable)
                ));
            } else {
                self.update.push(format!(
                    "\u{0275}\u{0275}property('{}', {});",
                    binding_name,
                    ctx_expr(dir_expr)
                ));
            }
        } else {
            self.update.push(format!(
                "\u{0275}\u{0275}property('{}', {});",
                dir_name,
                ctx_expr(dir_expr)
            ));
        }
        self.var_count += 1;
    }

    /// Generate a child template containing a single element.
    fn generate_child_template_with_element(
        &mut self,
        fn_name: &str,
        el: &ElementNode,
    ) -> ChildTemplate {
        let parent_slot = self.slot_index;
        let parent_var = self.var_count;
        let parent_pipe_offset = self.pipe_var_offset;
        let parent_last_update: Option<u32> = self.last_update_slot;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);
        let parent_consts = std::mem::take(&mut self.consts);
        let parent_lets = self.let_declarations.clone();
        let parent_refs = std::mem::take(&mut self.template_refs);
        // Each child template function runs with its own namespace flag that
        // starts as HTML, so reset the tracked state. The stack is left
        // intact — its top is the outer child_ns and remains the correct
        // inherited namespace for elements generated inside the child.
        let parent_ns_state = self.namespace_state;
        self.namespace_state = Namespace::Html;

        self.slot_index = 0;
        self.var_count = 0;
        self.pipe_var_offset = 0;
        self.last_update_slot = None;

        self.generate_element(el);

        let decls = self.slot_index;

        let mut code = format!("function {fn_name}(rf, ctx) {{\n");
        if !self.creation.is_empty() {
            code.push_str("  if (rf & 1) {\n");
            for instr in &self.creation {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        if !self.update.is_empty() || !parent_lets.is_empty() {
            code.push_str("  if (rf & 2) {\n");
            for (name, slot) in &parent_lets {
                self.ivy_imports
                    .insert("\u{0275}\u{0275}readContextLet".to_string());
                code.push_str(&format!(
                    "    const {name} = \u{0275}\u{0275}readContextLet({slot});\n"
                ));
            }
            for instr in &self.update {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        code.push('}');

        // Rewrite pipe offsets: compute sequential binding count for this child,
        // then shift all pipe offsets to not overlap with sequential bindings.
        let child_seq = count_sequential_bindings(&self.update);
        let child_pipe = count_pipe_binding_slots(&self.update);
        let code = rewrite_pipe_offsets(&code, child_seq);
        let vars = child_seq + child_pipe + parent_lets.len() as u32;

        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.pipe_var_offset = parent_pipe_offset;
        self.last_update_slot = parent_last_update;
        self.creation = parent_creation;
        self.update = parent_update;
        self.consts = parent_consts;
        self.let_declarations = parent_lets;
        self.template_refs = parent_refs;
        self.namespace_state = parent_ns_state;

        ChildTemplate {
            function_name: fn_name.to_string(),
            decls,
            vars,
            code,
        }
    }

    fn generate_text(&mut self, text: &TextNode) {
        let slot = self.slot_index;
        self.slot_index += 1;
        self.ivy_imports.insert("\u{0275}\u{0275}text".to_string());

        let decoded = decode_html_entities(&text.value);
        let escaped = escape_js_string(&decoded);
        self.creation
            .push(format!("\u{0275}\u{0275}text({slot}, '{escaped}');"));
    }

    fn generate_interpolation_with_suffix(&mut self, interp: &InterpolationNode, suffix: &str) {
        let slot = self.slot_index;
        self.slot_index += 1;
        self.ivy_imports.insert("\u{0275}\u{0275}text".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}textInterpolate1".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        self.creation.push(format!("\u{0275}\u{0275}text({slot});"));

        let expr = if interp.pipes.is_empty() {
            self.compile_binding_expr(&interp.expression)
        } else {
            self.wrap_with_pipes(&interp.expression, &interp.pipes)
        };

        let escaped_suffix = escape_js_string(suffix);
        self.add_advance(slot);
        self.update.push(format!(
            "\u{0275}\u{0275}textInterpolate1('', {expr}, '{escaped_suffix}');",
        ));
        // textInterpolate1 uses 1 binding slot
        self.var_count += 1;
    }

    fn generate_interpolation(&mut self, interp: &InterpolationNode) {
        let slot = self.slot_index;
        self.slot_index += 1;
        self.ivy_imports.insert("\u{0275}\u{0275}text".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}textInterpolate".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        self.creation.push(format!("\u{0275}\u{0275}text({slot});"));

        // Build the expression with pipe wrapping and nested pipe compilation
        let expr = if interp.pipes.is_empty() {
            self.compile_binding_expr(&interp.expression)
        } else {
            self.wrap_with_pipes(&interp.expression, &interp.pipes)
        };

        self.add_advance(slot);
        self.update
            .push(format!("\u{0275}\u{0275}textInterpolate({expr});"));
        self.var_count += 1;
    }

    fn generate_if_block(&mut self, block: &IfBlockNode) {
        let slot = self.slot_index;
        self.slot_index += 1;

        self.ivy_imports
            .insert("\u{0275}\u{0275}conditional".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        // Each independent @if block uses conditionalCreate.
        // Only @else-if and @else branches (within the SAME @if construct)
        // use conditionalCreate.
        self.ivy_imports
            .insert("\u{0275}\u{0275}conditionalCreate".to_string());
        let create_fn = "\u{0275}\u{0275}conditionalCreate";

        // Generate child template for the @if body
        let child_fn_name = format!(
            "{}_Conditional_{}_Template",
            self.component_name, self.child_counter
        );
        self.child_counter += 1;

        // Extract root element tag name and attributes for the conditional host element.
        // Angular 21 passes the first child's tag and consts index to conditionalCreate
        // so the container is backed by a real DOM element with proper attributes.
        let (root_tag, root_attrs_idx) = get_root_element_info(&block.children, self);
        let child = self.generate_child_template(&child_fn_name, &block.children);
        match (root_tag, root_attrs_idx) {
            (Some(ref tag), Some(idx)) => {
                self.creation.push(format!(
                    "{create_fn}({slot}, {child_fn_name}, {}, {}, '{tag}', {idx});",
                    child.decls, child.vars
                ));
            }
            (Some(ref tag), None) => {
                self.creation.push(format!(
                    "{create_fn}({slot}, {child_fn_name}, {}, {}, '{tag}');",
                    child.decls, child.vars
                ));
            }
            _ => {
                self.creation.push(format!(
                    "{create_fn}({slot}, {child_fn_name}, {}, {});",
                    child.decls, child.vars
                ));
            }
        }
        self.child_templates.push(child);

        // Generate else-if and else child templates
        let mut else_if_slots = Vec::new();
        for branch in &block.else_if_branches {
            let fn_name = format!(
                "{}_ConditionalElseIf_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            self.ivy_imports
                .insert("\u{0275}\u{0275}conditionalCreate".to_string());
            let ei_slot = self.slot_index;
            self.slot_index += 1;
            let (ei_tag, ei_attrs) = get_root_element_info(&branch.children, self);
            let child = self.generate_child_template(&fn_name, &branch.children);
            match (ei_tag, ei_attrs) {
                (Some(ref tag), Some(idx)) => self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({ei_slot}, {fn_name}, {}, {}, '{tag}', {idx});",
                    child.decls, child.vars
                )),
                (Some(ref tag), None) => self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({ei_slot}, {fn_name}, {}, {}, '{tag}');",
                    child.decls, child.vars
                )),
                _ => self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({ei_slot}, {fn_name}, {}, {});",
                    child.decls, child.vars
                )),
            }
            else_if_slots.push((branch.condition.clone(), fn_name.clone(), ei_slot));
            self.child_templates.push(child);
        }

        let mut else_slot_info = None;
        if let Some(ref else_children) = block.else_branch {
            let fn_name = format!(
                "{}_ConditionalElse_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            self.ivy_imports
                .insert("\u{0275}\u{0275}conditionalCreate".to_string());
            let else_slot = self.slot_index;
            self.slot_index += 1;
            let (else_tag, else_attrs) = get_root_element_info(else_children, self);
            let child = self.generate_child_template(&fn_name, else_children);
            match (else_tag, else_attrs) {
                (Some(ref tag), Some(idx)) => self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({else_slot}, {fn_name}, {}, {}, '{tag}', {idx});",
                    child.decls, child.vars
                )),
                (Some(ref tag), None) => self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({else_slot}, {fn_name}, {}, {}, '{tag}');",
                    child.decls, child.vars
                )),
                _ => self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({else_slot}, {fn_name}, {}, {});",
                    child.decls, child.vars
                )),
            }
            else_slot_info = Some((fn_name.clone(), else_slot));
            self.child_templates.push(child);
        }

        // Update block: conditional with absolute slot indices
        self.add_advance(slot);
        let cond_expr = build_conditional_expr(
            &block.condition,
            slot,
            &else_if_slots,
            &else_slot_info,
            &self.local_vars,
        );
        self.update
            .push(format!("\u{0275}\u{0275}conditional({cond_expr});"));
        self.var_count += 1;
    }

    fn generate_for_block(&mut self, block: &ForBlockNode) {
        let slot = self.slot_index;
        // ɵɵrepeaterCreate internally uses: slot for metadata, slot+1 for template,
        // and slot+2 for @empty template (if present).  Reserve all needed slots.
        let has_empty = block.empty_children.is_some();
        self.slot_index += if has_empty { 3 } else { 2 };

        self.ivy_imports
            .insert("\u{0275}\u{0275}repeaterCreate".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}repeater".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        let child_fn_name = format!(
            "{}_For_{}_Template",
            self.component_name, self.child_counter
        );
        self.child_counter += 1;

        let child =
            self.generate_for_child_template(&child_fn_name, &block.item_name, &block.children);
        // Build the track-by function from the track expression.
        // Angular expects: (index, item) => item.id
        // The track expression runs in the arrow function `(i, item) => expr`
        // where `item` is a local parameter — do NOT prefix it with `ctx.`.
        let mut track_locals = self.local_vars.clone();
        track_locals.insert(block.item_name.clone());
        let track_expr = ctx_expr_with_locals(&block.track_expression, &track_locals);
        let raw_track = &block.track_expression;
        let track_fn = if raw_track.trim() == "$index" {
            // track $index → identity by index
            self.ivy_imports
                .insert("\u{0275}\u{0275}repeaterTrackByIndex".to_string());
            "\u{0275}\u{0275}repeaterTrackByIndex".to_string()
        } else if raw_track.trim() == block.item_name {
            // track item → identity by reference
            self.ivy_imports
                .insert("\u{0275}\u{0275}repeaterTrackByIdentity".to_string());
            "\u{0275}\u{0275}repeaterTrackByIdentity".to_string()
        } else {
            // Custom track expression → wrap in arrow function
            let item = &block.item_name;
            format!("(i, {item}) => {track_expr}")
        };

        // Extract root element tag and consts index for the @for host element.
        let (for_tag, for_attrs_idx) = get_root_element_info(&block.children, self);
        let tag_arg = for_tag
            .as_ref()
            .map(|t| format!("'{t}'"))
            .unwrap_or_else(|| "null".to_string());
        let attrs_arg = for_attrs_idx
            .map(|i| i.to_string())
            .unwrap_or_else(|| "null".to_string());

        // @empty block — passed as extra args to ɵɵrepeaterCreate
        if let Some(ref empty_children) = block.empty_children {
            let empty_fn_name = format!("{}_ForEmpty_{}_Template", self.component_name, slot);
            let empty_child = self.generate_child_template(&empty_fn_name, empty_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}repeaterCreate({slot}, {child_fn_name}, {}, {}, {tag_arg}, {attrs_arg}, {track_fn}, false, {empty_fn_name}, {}, {});",
                child.decls, child.vars, empty_child.decls, empty_child.vars
            ));
            self.child_templates.push(empty_child);
        } else {
            self.creation.push(format!(
                "\u{0275}\u{0275}repeaterCreate({slot}, {child_fn_name}, {}, {}, {tag_arg}, {attrs_arg}, {track_fn});",
                child.decls, child.vars
            ));
        }
        self.child_templates.push(child);

        self.add_advance(slot);
        self.update.push(format!(
            "\u{0275}\u{0275}repeater({});",
            ctx_expr_with_locals(&block.iterable, &self.local_vars)
        ));
        self.var_count += 1;
    }

    fn generate_switch_block(&mut self, block: &SwitchBlockNode) {
        // Similar to @if with multiple conditional branches
        let slot = self.slot_index;
        self.slot_index += 1;

        self.ivy_imports
            .insert("\u{0275}\u{0275}conditionalCreate".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}conditional".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        let mut case_slots = Vec::new();
        let is_first_case = true;
        for (i, case) in block.cases.iter().enumerate() {
            let fn_name = format!(
                "{}_SwitchCase_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            let case_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, &case.children);
            if i == 0 && is_first_case {
                self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({case_slot}, {fn_name}, {}, {});",
                    child.decls, child.vars
                ));
            } else {
                self.ivy_imports
                    .insert("\u{0275}\u{0275}conditionalCreate".to_string());
                self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalCreate({case_slot}, {fn_name}, {}, {});",
                    child.decls, child.vars
                ));
            }
            case_slots.push((case.expression.clone(), case_slot));
            self.child_templates.push(child);
        }

        let mut default_slot_val = None;
        if let Some(ref default_children) = block.default_branch {
            let fn_name = format!(
                "{}_SwitchDefault_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            self.ivy_imports
                .insert("\u{0275}\u{0275}conditionalCreate".to_string());
            let default_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, default_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}conditionalCreate({default_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
            default_slot_val = Some(default_slot);
            self.child_templates.push(child);
        }

        self.add_advance(slot);
        // Build switch conditional expression with absolute slot indices
        let mut cond = String::new();
        for (i, (expr, case_slot)) in case_slots.iter().enumerate() {
            if i > 0 {
                cond.push_str(" : ");
            }
            cond.push_str(&format!(
                "{} === {} ? {}",
                ctx_expr_with_locals(&block.expression, &self.local_vars),
                expr,
                case_slot
            ));
        }
        if let Some(ds) = default_slot_val {
            cond.push_str(&format!(" : {ds}"));
        } else {
            cond.push_str(" : -1");
        }
        self.update
            .push(format!("\u{0275}\u{0275}conditional({cond});"));
        self.var_count += 1;
    }

    fn generate_child_template(
        &mut self,
        fn_name: &str,
        children: &[TemplateNode],
    ) -> ChildTemplate {
        // Save parent state.  Note: self.consts is NOT saved/restored — all
        // templates within a component share one consts array (tView.consts),
        // so child template entries must accumulate in the same vec.
        let parent_slot = self.slot_index;
        let parent_var = self.var_count;
        let parent_pipe_offset = self.pipe_var_offset;
        let parent_last_update: Option<u32> = self.last_update_slot;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);
        let parent_lets = self.let_declarations.clone();
        let parent_refs = std::mem::take(&mut self.template_refs);
        self.scope_stack.push(ScopeEntry::Conditional);
        // Reset namespace_state for this child template function (its runtime
        // namespace flag starts as HTML). The stack is left intact so nested
        // elements inherit the outer context's namespace.
        let parent_ns_state = self.namespace_state;
        self.namespace_state = Namespace::Html;

        self.slot_index = 0;
        self.var_count = 0;
        self.pipe_var_offset = 0;
        self.last_update_slot = None;
        // Don't clear let_declarations, local_vars, or consts — children
        // inherit parent scope and share the component-level consts array.
        // template_refs, however, is scoped per-template: refs live in a
        // specific LView's slot space and can't cross TView boundaries.

        self.generate_nodes(children);

        let decls = self.slot_index;

        // Embedded views (conditional / repeater child templates) do not
        // receive the parent component as `ctx`.  We must use ɵɵnextContext()
        // in the update block and ɵɵrestoreView + ɵɵnextContext in listeners.
        let has_listeners = self.creation.iter().any(|s| s.contains("listener"));

        // Use `_ctx` as parameter name since we rebind `ctx` via ɵɵnextContext()
        // in the update block.  Using `ctx` for both would be a const redeclaration.
        let mut code = format!("function {fn_name}(rf, _ctx) {{\n");
        if !self.creation.is_empty() {
            code.push_str("  if (rf & 1) {\n");
            if has_listeners {
                self.ivy_imports
                    .insert("\u{0275}\u{0275}getCurrentView".to_string());
                code.push_str("    const _r = \u{0275}\u{0275}getCurrentView();\n");
            }
            for instr in &self.creation {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        if !self.update.is_empty() || !parent_lets.is_empty() {
            code.push_str("  if (rf & 2) {\n");
            self.ivy_imports
                .insert("\u{0275}\u{0275}nextContext".to_string());
            // Template-reference reads MUST come before ɵɵnextContext —
            // ɵɵreference(slot) reads the current context LView, and
            // nextContext switches that context to the parent.
            code.push_str(&self.build_ref_reads_prelude("    "));
            // Use the scope stack to generate context navigation
            code.push_str(&self.generate_context_navigation());
            // Inject @let variable reads from parent context
            for (name, slot) in &parent_lets {
                self.ivy_imports
                    .insert("\u{0275}\u{0275}readContextLet".to_string());
                code.push_str(&format!(
                    "    const {name} = \u{0275}\u{0275}readContextLet({slot});\n"
                ));
            }
            for instr in &self.update {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        code.push('}');

        // Rewrite pipe offsets: shift pipe varSlot values so they don't overlap
        // with sequential bindings.  Must happen before self.update is restored.
        let child_seq = count_sequential_bindings(&self.update);
        let child_pipe = count_pipe_binding_slots(&self.update);
        let code = rewrite_pipe_offsets(&code, child_seq);
        let vars = child_seq + child_pipe + parent_lets.len() as u32;

        // Restore parent state (consts is NOT restored — shared)
        self.scope_stack.pop();
        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.pipe_var_offset = parent_pipe_offset;
        self.last_update_slot = parent_last_update;
        self.creation = parent_creation;
        self.update = parent_update;
        self.let_declarations = parent_lets;
        self.template_refs = parent_refs;
        self.namespace_state = parent_ns_state;

        ChildTemplate {
            function_name: fn_name.to_string(),
            decls,
            vars,
            code,
        }
    }

    fn generate_for_child_template(
        &mut self,
        fn_name: &str,
        item_name: &str,
        children: &[TemplateNode],
    ) -> ChildTemplate {
        // Save parent state (consts is NOT saved — shared component-level array)
        let parent_slot = self.slot_index;
        let parent_var = self.var_count;
        let parent_pipe_offset = self.pipe_var_offset;
        let parent_last_update: Option<u32> = self.last_update_slot;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);
        let parent_lets = self.let_declarations.clone();
        let parent_refs = std::mem::take(&mut self.template_refs);

        self.slot_index = 0;
        self.var_count = 0;
        self.pipe_var_offset = 0;
        self.last_update_slot = None;
        // Reset namespace_state for this child template function; stack is
        // left intact so @for row elements inherit the outer namespace.
        let parent_ns_state = self.namespace_state;
        self.namespace_state = Namespace::Html;

        // Register the @for item variable AND the implicit built-ins
        // (`$index`, `$count`, `$first`, `$last`, `$even`, `$odd`) as locals
        // so ctx_expr_with_locals() does NOT prefix them with `ctx.`. Each is
        // declared at runtime from the embedded view's `_ctx` only when the
        // body actually references it (matches ng build's emit and avoids
        // bouncing through the parent component for purely local reads).
        let parent_locals = self.local_vars.clone();
        self.local_vars.insert(item_name.to_string());
        for builtin in FOR_BUILTIN_LOCALS {
            self.local_vars.insert((*builtin).to_string());
        }
        // Track this @for's item variable and its depth for nested templates
        self.scope_stack.push(ScopeEntry::Repeater {
            item_name: item_name.to_string(),
        });

        self.generate_nodes(children);

        let decls = self.slot_index;

        let has_listeners = self.creation.iter().any(|s| s.contains("listener"));

        let mut code = format!("function {fn_name}(rf, _ctx) {{\n");
        if !self.creation.is_empty() {
            code.push_str("  if (rf & 1) {\n");
            if has_listeners {
                self.ivy_imports
                    .insert("\u{0275}\u{0275}getCurrentView".to_string());
                code.push_str("    const _r = \u{0275}\u{0275}getCurrentView();\n");
            }
            for instr in &self.creation {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        if !self.update.is_empty() || !parent_lets.is_empty() {
            code.push_str("  if (rf & 2) {\n");
            // Decide which embedded-view locals the body actually reads.
            // Without this, every @for body unconditionally extracted the
            // loop item and called ɵɵnextContext(N) — the latter shifted
            // `ctx` to the parent component and broke `$index` / item
            // resolution for purely local references (issue #91).
            let body_text = self.update.join("\n");
            let needs_item = identifier_used_in(&body_text, item_name);
            let mut needed_builtins: Vec<&str> = Vec::new();
            for builtin in FOR_BUILTIN_LOCALS {
                if identifier_used_in(&body_text, builtin) {
                    needed_builtins.push(*builtin);
                }
            }
            let needs_parent_ctx = update_references_parent_ctx(&self.update);

            if needs_item {
                code.push_str(&format!("    const {item_name} = _ctx.$implicit;\n"));
            }
            for builtin in &needed_builtins {
                code.push_str(&format!("    const {builtin} = _ctx.{builtin};\n"));
            }
            // Template-reference reads must run BEFORE ɵɵnextContext —
            // ɵɵreference reads the current context LView.
            code.push_str(&self.build_ref_reads_prelude("    "));
            // Only walk to the parent component when something actually
            // needs it (a parent-scope binding or a `@let` resolved via
            // ɵɵreadContextLet, which itself takes the surrounding
            // context as its base).
            if needs_parent_ctx || !parent_lets.is_empty() {
                let comp_depth = self.scope_stack.len() as u32;
                self.ivy_imports
                    .insert("\u{0275}\u{0275}nextContext".to_string());
                if comp_depth > 1 {
                    code.push_str(&format!(
                        "    const ctx = \u{0275}\u{0275}nextContext({comp_depth});\n"
                    ));
                } else {
                    code.push_str("    const ctx = \u{0275}\u{0275}nextContext();\n");
                }
            }
            for (name, slot) in &parent_lets {
                self.ivy_imports
                    .insert("\u{0275}\u{0275}readContextLet".to_string());
                code.push_str(&format!(
                    "    const {name} = \u{0275}\u{0275}readContextLet({slot});\n"
                ));
            }
            for instr in &self.update {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        code.push('}');

        // Rewrite pipe offsets: shift pipe varSlot values so they don't overlap
        // with sequential bindings.  Must happen before self.update is restored.
        let child_seq = count_sequential_bindings(&self.update);
        let child_pipe = count_pipe_binding_slots(&self.update);
        let code = rewrite_pipe_offsets(&code, child_seq);
        let vars = child_seq + child_pipe + parent_lets.len() as u32;

        // Restore parent state (consts is NOT restored — shared)
        self.scope_stack.pop();
        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.pipe_var_offset = parent_pipe_offset;
        self.last_update_slot = parent_last_update;
        self.creation = parent_creation;
        self.update = parent_update;
        self.let_declarations = parent_lets;
        self.local_vars = parent_locals;
        self.template_refs = parent_refs;
        self.namespace_state = parent_ns_state;

        ChildTemplate {
            function_name: fn_name.to_string(),
            decls,
            vars,
            code,
        }
    }

    fn wrap_with_pipes(&mut self, base_expr: &str, pipes: &[PipeCall]) -> String {
        let mut expr = self.compile_binding_expr(base_expr);
        for pipe in pipes {
            let pipe_slot = self.slot_index;
            self.slot_index += 1;
            // Use the pipe-specific offset counter so pipe binding slots are
            // contiguous (0, 2, 5, …).  rewrite_pipe_offsets later shifts these
            // by the total sequential binding count.
            let pipe_var_slot = self.pipe_var_offset;
            let slots = 2 + pipe.args.len() as u32;
            self.pipe_var_offset += slots;
            self.var_count += slots;

            self.ivy_imports.insert("\u{0275}\u{0275}pipe".to_string());
            self.creation.push(format!(
                "\u{0275}\u{0275}pipe({pipe_slot}, '{}');",
                pipe.name
            ));

            let bind_fn = match pipe.args.len() {
                0 => "\u{0275}\u{0275}pipeBind1".to_string(),
                1 => "\u{0275}\u{0275}pipeBind2".to_string(),
                2 => "\u{0275}\u{0275}pipeBind3".to_string(),
                _ => "\u{0275}\u{0275}pipeBindV".to_string(),
            };
            self.ivy_imports.insert(bind_fn.clone());
            if pipe.args.is_empty() {
                expr = format!("{bind_fn}({pipe_slot}, {pipe_var_slot}, {expr})");
            } else {
                let compiled_args: Vec<String> = pipe
                    .args
                    .iter()
                    .map(|a| compile_pipe_arg(a, &self.local_vars))
                    .collect();
                expr = format!(
                    "{bind_fn}({pipe_slot}, {pipe_var_slot}, {expr}, {})",
                    compiled_args.join(", ")
                );
            }
        }
        expr
    }

    /// Compile a binding expression, handling embedded Angular pipes at any depth.
    ///
    /// Scans for `expr | pipeName` patterns (Angular pipe syntax) anywhere in the
    /// expression, compiles each to a `ɵɵpipeBind*` call, and applies `ctx.` prefixes.
    /// Generate an `@defer` block with its `@placeholder` / `@loading` /
    /// `@error` sub-blocks and trigger instructions.
    ///
    /// Emits per the issue spec:
    /// `ɵɵdefer(index, dependencyFn, loadingTmpl?, placeholderTmpl?, errorTmpl?)`
    /// followed by the trigger instructions:
    /// - `on viewport|idle|hover|interaction|immediate` → `ɵɵdeferOn*`
    /// - `on timer(<duration>)` → `ɵɵdeferOnTimer(<ms>)`
    /// - `when <expr>` → `ɵɵdeferWhen(ctx.<expr>)` in the update block
    /// - prefetch variants → `ɵɵdeferPrefetchOn*` / `ɵɵdeferPrefetchWhen`
    ///
    /// Slot layout:
    /// - `index` — the defer block slot (used by the runtime for state).
    /// - `index + 1` — primary (main) template.
    /// - `index + 2..` — placeholder, loading, error templates (in that order,
    ///   if present).
    ///
    /// The dependency function is emitted as a module-level helper that
    /// returns an array of dynamic `import()` promises — one per custom
    /// element tag inside the defer block that matches a `imports_identifiers`
    /// entry on the component. When no match is found the array is empty.
    fn generate_defer_block(&mut self, block: &DeferBlockNode) {
        let defer_slot = self.slot_index;
        self.slot_index += 1;

        self.ivy_imports.insert("\u{0275}\u{0275}defer".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}template".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        // Main template (always present).
        let main_slot = self.slot_index;
        self.slot_index += 1;
        let main_fn = format!(
            "{}_Defer_{}_Template",
            self.component_name, self.child_counter
        );
        self.child_counter += 1;
        let main_child = self.generate_child_template(&main_fn, &block.children);

        // Optional sub-block templates.
        let placeholder_info = block.placeholder.as_ref().map(|children| {
            let slot = self.slot_index;
            self.slot_index += 1;
            let fn_name = format!(
                "{}_DeferPlaceholder_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            let child = self.generate_child_template(&fn_name, children);
            (slot, fn_name, child)
        });
        let loading_info = block.loading.as_ref().map(|children| {
            let slot = self.slot_index;
            self.slot_index += 1;
            let fn_name = format!(
                "{}_DeferLoading_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            let child = self.generate_child_template(&fn_name, children);
            (slot, fn_name, child)
        });
        let error_info = block.error.as_ref().map(|children| {
            let slot = self.slot_index;
            self.slot_index += 1;
            let fn_name = format!(
                "{}_DeferError_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            let child = self.generate_child_template(&fn_name, children);
            (slot, fn_name, child)
        });

        // Build the dependency resolver function. Emitted as a module-scope
        // helper so the bundler's import scanner can pick up any `import()`
        // calls inside it as split points.
        let deps_fn_name = format!(
            "{}_Defer_{}_DepsFn",
            self.component_name, self.child_counter
        );
        self.child_counter += 1;
        let deps_fn_code = build_defer_deps_fn(&deps_fn_name, &block.children);
        self.child_templates.push(ChildTemplate {
            function_name: deps_fn_name.clone(),
            decls: 0,
            vars: 0,
            code: deps_fn_code,
        });

        // Emit ɵɵtemplate(slot, fn, decls, vars) for each template.
        self.creation.push(format!(
            "\u{0275}\u{0275}template({main_slot}, {main_fn}, {}, {});",
            main_child.decls, main_child.vars
        ));
        self.child_templates.push(main_child);

        if let Some((slot, fn_name, child)) = &placeholder_info {
            self.creation.push(format!(
                "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
        }
        if let Some((slot, fn_name, child)) = &loading_info {
            self.creation.push(format!(
                "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
        }
        if let Some((slot, fn_name, child)) = &error_info {
            self.creation.push(format!(
                "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
        }

        // Build the ɵɵdefer call, following the issue's signature:
        //   ɵɵdefer(index, dependencyFn, loadingTmpl?, placeholderTmpl?, errorTmpl?)
        let loading_arg = loading_info
            .as_ref()
            .map(|(s, _, _)| s.to_string())
            .unwrap_or_else(|| "null".to_string());
        let placeholder_arg = placeholder_info
            .as_ref()
            .map(|(s, _, _)| s.to_string())
            .unwrap_or_else(|| "null".to_string());
        let error_arg = error_info
            .as_ref()
            .map(|(s, _, _)| s.to_string())
            .unwrap_or_else(|| "null".to_string());
        self.creation.push(format!(
            "\u{0275}\u{0275}defer({defer_slot}, {deps_fn_name}, {loading_arg}, {placeholder_arg}, {error_arg});"
        ));

        // Move the child templates for placeholder/loading/error into the
        // component's child_templates vec. They were produced inside each
        // `Some(...)` branch but need to live alongside the others.
        if let Some((_, _, child)) = placeholder_info {
            self.child_templates.push(child);
        }
        if let Some((_, _, child)) = loading_info {
            self.child_templates.push(child);
        }
        if let Some((_, _, child)) = error_info {
            self.child_templates.push(child);
        }

        // Trigger instructions — emit for each trigger, prefetch variants last.
        for trig in &block.triggers {
            self.emit_defer_trigger(trig, defer_slot, false);
        }
        for trig in &block.prefetch_triggers {
            self.emit_defer_trigger(trig, defer_slot, true);
        }
    }

    /// Emit a single trigger instruction for an `@defer` block. `on`-family
    /// triggers land in the creation block; `when` triggers land in the update
    /// block with an `ɵɵadvance` to the defer slot.
    fn emit_defer_trigger(&mut self, trig: &DeferTrigger, defer_slot: u32, prefetch: bool) {
        match trig {
            DeferTrigger::Viewport(_) => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchOnViewport"
                } else {
                    "\u{0275}\u{0275}deferOnViewport"
                };
                self.ivy_imports.insert(sym.to_string());
                self.creation.push(format!("{sym}();"));
            }
            DeferTrigger::Idle => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchOnIdle"
                } else {
                    "\u{0275}\u{0275}deferOnIdle"
                };
                self.ivy_imports.insert(sym.to_string());
                self.creation.push(format!("{sym}();"));
            }
            DeferTrigger::Immediate => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchOnImmediate"
                } else {
                    "\u{0275}\u{0275}deferOnImmediate"
                };
                self.ivy_imports.insert(sym.to_string());
                self.creation.push(format!("{sym}();"));
            }
            DeferTrigger::Hover(_) => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchOnHover"
                } else {
                    "\u{0275}\u{0275}deferOnHover"
                };
                self.ivy_imports.insert(sym.to_string());
                self.creation.push(format!("{sym}();"));
            }
            DeferTrigger::Interaction(_) => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchOnInteraction"
                } else {
                    "\u{0275}\u{0275}deferOnInteraction"
                };
                self.ivy_imports.insert(sym.to_string());
                self.creation.push(format!("{sym}();"));
            }
            DeferTrigger::Timer(duration) => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchOnTimer"
                } else {
                    "\u{0275}\u{0275}deferOnTimer"
                };
                self.ivy_imports.insert(sym.to_string());
                let ms = parse_duration_to_ms(duration);
                self.creation.push(format!("{sym}({ms});"));
            }
            DeferTrigger::When(expr) => {
                let sym = if prefetch {
                    "\u{0275}\u{0275}deferPrefetchWhen"
                } else {
                    "\u{0275}\u{0275}deferWhen"
                };
                self.ivy_imports.insert(sym.to_string());
                let compiled = self.compile_binding_expr(expr);
                self.add_advance(defer_slot);
                self.update.push(format!("{sym}({compiled});"));
                self.var_count += 1;
            }
        }
    }

    /// Generate an `@let` variable declaration.
    fn generate_let_declaration(&mut self, decl: &LetDeclarationNode) {
        let slot = self.slot_index;
        self.slot_index += 1;

        self.ivy_imports
            .insert("\u{0275}\u{0275}declareLet".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}storeLet".to_string());

        // Creation mode: allocate the let slot
        self.creation
            .push(format!("\u{0275}\u{0275}declareLet({slot});"));

        // Update mode: evaluate expression and store the value
        let compiled_expr = self.compile_binding_expr(&decl.expression);
        self.update.push(format!(
            "const {} = \u{0275}\u{0275}storeLet({compiled_expr});",
            decl.name
        ));

        // Track for child templates and ctx. prefix exclusion
        self.let_declarations.push((decl.name.clone(), slot));
        self.local_vars.insert(decl.name.clone());

        // storeLet counts as one binding var
        self.var_count += 1;
    }

    fn compile_binding_expr(&mut self, expression: &str) -> String {
        let segments = extract_all_pipe_segments(expression);
        if segments.is_empty() {
            // No pipes found — just compile with ctx. prefix.
            // Template-reference names are in `local_vars` so they're left
            // unprefixed; the `const <name> = ɵɵreference(<slot>);` prelude
            // at the top of the update block resolves them at runtime.
            return ctx_expr_with_locals(expression, &self.local_vars);
        }
        self.replace_pipes_in_expr(expression)
    }

    /// Replace all `expr | pipeName` patterns in an expression with `ɵɵpipeBind*` calls.
    fn replace_pipes_in_expr(&mut self, expression: &str) -> String {
        let trimmed = expression.trim();

        // Check for top-level pipe: `baseExpr | pipeName : arg1 : arg2`
        if let Some((base, pipe_name, args)) = split_top_level_pipe_with_args(trimmed) {
            let compiled_base = self.replace_pipes_in_expr(&base);
            let pipe_slot = self.slot_index;
            self.slot_index += 1;
            // Use the pipe-specific offset counter so pipe binding slots are
            // packed contiguously (0, 2, 5, …) across the whole template.
            // `rewrite_pipe_offsets` later shifts these by the sequential
            // binding count so they sit after all sequential bindings in LView.
            let pipe_var_slot = self.pipe_var_offset;
            let slots = 2 + args.len() as u32;
            self.pipe_var_offset += slots;
            self.var_count += slots;
            self.ivy_imports.insert("\u{0275}\u{0275}pipe".to_string());

            let bind_fn = match args.len() {
                0 => "\u{0275}\u{0275}pipeBind1",
                1 => "\u{0275}\u{0275}pipeBind2",
                2 => "\u{0275}\u{0275}pipeBind3",
                _ => "\u{0275}\u{0275}pipeBindV",
            };
            self.ivy_imports.insert(bind_fn.to_string());
            self.creation.push(format!(
                "\u{0275}\u{0275}pipe({pipe_slot}, '{}');",
                pipe_name
            ));
            if args.is_empty() {
                return format!("{bind_fn}({pipe_slot}, {pipe_var_slot}, {compiled_base})");
            }
            let compiled_args: Vec<String> = args
                .iter()
                .map(|a| compile_pipe_arg(a, &self.local_vars))
                .collect();
            return format!(
                "{bind_fn}({pipe_slot}, {pipe_var_slot}, {compiled_base}, {})",
                compiled_args.join(", ")
            );
        }

        // No top-level pipe — scan for `(expr | pipe)` sub-expressions and replace them.
        // After pipe replacement, apply ctx_expr with locals that include `ctx` itself
        // (pipe compilation already prefixed inner values with `ctx.`).
        let result = replace_nested_pipe_parens(trimmed, self);
        let mut locals = self.local_vars.clone();
        locals.insert("ctx".to_string());
        ctx_expr_with_locals(&result, &locals)
    }

    /// Register a static attribute array in the `consts` table and return its index.
    fn register_const(&mut self, attrs: &[(&str, &str)]) -> usize {
        let formatted = format_static_attrs(attrs);
        let idx = self.consts.len();
        self.consts.push(formatted);
        idx
    }

    /// Register a consts entry that includes both static attributes and binding markers.
    fn register_const_with_bindings(
        &mut self,
        attrs: &[(&str, &str)],
        binding_names: &[&str],
    ) -> usize {
        let formatted = format_attrs_with_bindings(attrs, binding_names);
        let idx = self.consts.len();
        self.consts.push(formatted);
        idx
    }

    /// Register a consts entry describing the template-reference variables
    /// on an element, e.g. `['profileForm', 'ngForm']` or `['fileInput', '']`.
    /// Multiple refs on the same element flatten into a single entry:
    /// `['a', '', 'b', 'bDirective']`.
    fn register_refs_const(&mut self, refs: &[(String, String)]) -> usize {
        let mut parts = Vec::with_capacity(refs.len() * 2);
        for (name, export_as) in refs {
            parts.push(format!("'{}'", escape_js_string(name)));
            parts.push(format!("'{}'", escape_js_string(export_as)));
        }
        let formatted = format!("[{}]", parts.join(", "));
        let idx = self.consts.len();
        self.consts.push(formatted);
        idx
    }

    /// Add an `ɵɵadvance()` instruction to the update block.
    /// Generate context navigation code for a child template's update block.
    ///
    /// Walks the scope stack from the current scope up to the component,
    /// extracting @for item variables along the way and binding `ctx` to
    /// the component context. Returns the code to inject at the top of
    /// the `if (rf & 2)` block.
    fn generate_context_navigation(&self) -> String {
        if self.scope_stack.is_empty() {
            return String::new(); // root template — ctx is the function parameter
        }

        let _ = self.ivy_imports.clone(); // can't mutate, but we need imports
        let mut code = String::new();
        let depth = self.scope_stack.len();
        let mut levels_consumed = 0;

        // Walk the scope stack from innermost (current) to outermost.
        // The stack represents [outermost, ..., innermost], so we iterate in reverse.
        // i=0 is the innermost scope (the one we're currently IN).
        // To reach scope at reverse-index i, we need i navigation steps
        // (minus any already consumed).
        for (i, entry) in self.scope_stack.iter().rev().enumerate() {
            match entry {
                ScopeEntry::Repeater { item_name } if i > 0 => {
                    // This ANCESTOR is a @for — extract its item variable.
                    // (i=0 would be the current @for, handled by _ctx.$implicit)
                    let steps = i - levels_consumed;
                    if steps == 1 {
                        code.push_str(&format!(
                            "    const _{item_name}_ctx = \u{0275}\u{0275}nextContext();\n"
                        ));
                    } else if steps > 1 {
                        code.push_str(&format!(
                            "    const _{item_name}_ctx = \u{0275}\u{0275}nextContext({steps});\n"
                        ));
                    }
                    if steps > 0 {
                        code.push_str(&format!(
                            "    const {item_name} = _{item_name}_ctx.$implicit;\n"
                        ));
                    }
                    levels_consumed = i;
                }
                ScopeEntry::Repeater { .. } => {
                    // i=0: this is the current @for scope — item accessed via _ctx.$implicit
                }
                ScopeEntry::Conditional => {
                    // Skip — no variables to extract from conditional scopes
                }
            }
        }

        // Navigate remaining levels to the component
        let remaining = depth - levels_consumed;
        if remaining <= 1 {
            code.push_str("    const ctx = \u{0275}\u{0275}nextContext();\n");
        } else {
            code.push_str(&format!(
                "    const ctx = \u{0275}\u{0275}nextContext({remaining});\n"
            ));
        }

        code
    }

    /// Get the total scope depth (for listener closures that need `nextContext(N)`).
    fn scope_depth(&self) -> u32 {
        self.scope_stack.len() as u32
    }

    /// Generate the preamble code for a listener closure inside an embedded view.
    ///
    /// Produces: `ɵɵrestoreView(_r); const inner = _ctx.$implicit;
    /// [const _outer_ctx = ɵɵnextContext(); const outer = _outer_ctx.$implicit; …]
    /// const ctx = ɵɵnextContext(N);`
    ///
    /// Walks the full `scope_stack` so listeners inside nested `@for` blocks
    /// see every in-scope loop variable — mirrors `generate_context_navigation`
    /// with listener-appropriate single-line formatting.
    fn generate_listener_preamble(&self) -> String {
        let mut code = String::from("\u{0275}\u{0275}restoreView(_r); ");
        // Template-reference reads must come BEFORE ɵɵnextContext —
        // `ɵɵreference(slot)` uses the current context LView, and
        // nextContext switches that context to the parent.
        for (name, slot) in &self.template_refs {
            code.push_str(&format!(
                "const {name} = \u{0275}\u{0275}reference({slot}); "
            ));
        }
        if self.scope_stack.is_empty() {
            return code;
        }

        let depth = self.scope_stack.len();
        let mut levels_consumed: usize = 0;

        // Walk innermost → outermost. i=0 is the current scope (use _ctx directly,
        // the closure-captured template function parameter). i>0 are ancestor scopes
        // reached via ɵɵnextContext(steps).
        for (i, entry) in self.scope_stack.iter().rev().enumerate() {
            match entry {
                ScopeEntry::Repeater { item_name } if i == 0 => {
                    code.push_str(&format!("const {item_name} = _ctx.$implicit; "));
                }
                ScopeEntry::Repeater { item_name } => {
                    let steps = i - levels_consumed;
                    if steps == 1 {
                        code.push_str(&format!(
                            "const _{item_name}_ctx = \u{0275}\u{0275}nextContext(); "
                        ));
                    } else if steps > 1 {
                        code.push_str(&format!(
                            "const _{item_name}_ctx = \u{0275}\u{0275}nextContext({steps}); "
                        ));
                    }
                    if steps > 0 {
                        code.push_str(&format!("const {item_name} = _{item_name}_ctx.$implicit; "));
                    }
                    levels_consumed = i;
                }
                ScopeEntry::Conditional => {
                    // No implicit variable — still counts toward navigation depth.
                }
            }
        }

        let remaining = depth - levels_consumed;
        if remaining <= 1 {
            code.push_str("const ctx = \u{0275}\u{0275}nextContext(); ");
        } else {
            code.push_str(&format!(
                "const ctx = \u{0275}\u{0275}nextContext({remaining}); "
            ));
        }

        code
    }

    fn add_advance(&mut self, target_slot: u32) {
        // Angular's executeTemplate() sets selectedIndex = HEADER_OFFSET before the
        // update phase.  ɵɵadvance(delta) adds delta to that base, so the first call
        // must use delta = target_slot (not target_slot + 1) to reach
        // HEADER_OFFSET + target_slot, which is the correct LView index.
        let delta = match self.last_update_slot {
            None => target_slot,
            Some(last) => target_slot.saturating_sub(last),
        };
        if delta > 0 {
            self.ivy_imports
                .insert("\u{0275}\u{0275}advance".to_string());
            if delta == 1 {
                self.update.push("\u{0275}\u{0275}advance();".to_string());
            } else {
                self.update
                    .push(format!("\u{0275}\u{0275}advance({delta});"));
            }
        }
        self.last_update_slot = Some(target_slot);
    }

    /// Emit property/class/style/attr bindings for an element in the update block.
    fn emit_element_bindings(&mut self, el: &ElementNode, slot: u32) {
        let has_bindings = el.attributes.iter().any(|a| {
            matches!(
                a,
                TemplateAttribute::Property { .. }
                    | TemplateAttribute::ClassBinding { .. }
                    | TemplateAttribute::StyleBinding { .. }
                    | TemplateAttribute::AttrBinding { .. }
                    | TemplateAttribute::TwoWayBinding { .. }
            )
        });
        if has_bindings {
            self.add_advance(slot);
        }
        for attr in &el.attributes {
            match attr {
                TemplateAttribute::Property { name, expression } => {
                    let compiled = self.compile_binding_expr(expression);
                    if name == "class" {
                        // [class]="expr" → ɵɵclassMap(expr), 2 binding slots
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}classMap".to_string());
                        self.update
                            .push(format!("\u{0275}\u{0275}classMap({compiled});"));
                        self.var_count += 2;
                    } else if name == "style" {
                        // [style]="expr" → ɵɵstyleMap(expr), 2 binding slots
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}styleMap".to_string());
                        self.update
                            .push(format!("\u{0275}\u{0275}styleMap({compiled});"));
                        self.var_count += 2;
                    } else if let Some(attr_name) = name.strip_prefix("attr.") {
                        // [attr.X]="expr" → ɵɵattribute('X', expr)
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}attribute".to_string());
                        self.update.push(format!(
                            "\u{0275}\u{0275}attribute('{}', {compiled});",
                            attr_name
                        ));
                        self.var_count += 1;
                    } else if let Some(style_prop) = name.strip_prefix("style.") {
                        // [style.X]="expr" → ɵɵstyleProp('X', expr)
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}styleProp".to_string());
                        self.update.push(format!(
                            "\u{0275}\u{0275}styleProp('{}', {compiled});",
                            style_prop
                        ));
                        self.var_count += 2;
                    } else if let Some(class_name) = name.strip_prefix("class.") {
                        // [class.X]="expr" → ɵɵclassProp('X', expr)
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}classProp".to_string());
                        self.update.push(format!(
                            "\u{0275}\u{0275}classProp('{}', {compiled});",
                            class_name
                        ));
                        self.var_count += 2;
                    } else {
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}property".to_string());
                        self.update
                            .push(format!("\u{0275}\u{0275}property('{}', {compiled});", name));
                        self.var_count += 1;
                    }
                }
                TemplateAttribute::TwoWayBinding { name, expression } => {
                    // Two-way binding `[(x)]="expr"` must dispatch to
                    // the signal-aware `ɵɵtwoWayProperty` instead of
                    // `ɵɵproperty`. When `expr` is a `WritableSignal`,
                    // `ɵɵtwoWayProperty` unwraps it (calls the signal
                    // to read the value); for plain values it behaves
                    // like `ɵɵproperty`. Emitting `ɵɵproperty(name,
                    // signalRef)` would pass the signal OBJECT into
                    // the child input, so the child sees the wrapper
                    // rather than the value and template reads
                    // (`signal()`) on the parent later see whatever
                    // the listener wrote (likely a non-callable).
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}twoWayProperty".to_string());
                    let compiled = self.compile_binding_expr(expression);
                    self.update.push(format!(
                        "\u{0275}\u{0275}twoWayProperty('{}', {compiled});",
                        name,
                    ));
                    self.var_count += 1;
                }
                TemplateAttribute::ClassBinding {
                    class_name,
                    expression,
                } => {
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}classProp".to_string());
                    let compiled = self.compile_binding_expr(expression);
                    self.update.push(format!(
                        "\u{0275}\u{0275}classProp('{}', {compiled});",
                        class_name,
                    ));
                    self.var_count += 2; // Angular 21: style/class bindings use 2 slots
                }
                TemplateAttribute::StyleBinding {
                    property,
                    expression,
                } => {
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}styleProp".to_string());
                    let compiled = self.compile_binding_expr(expression);
                    self.update.push(format!(
                        "\u{0275}\u{0275}styleProp('{}', {compiled});",
                        property,
                    ));
                    self.var_count += 2; // Angular 21: style/class bindings use 2 slots
                }
                TemplateAttribute::AttrBinding { name, expression } => {
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}attribute".to_string());
                    let compiled = self.compile_binding_expr(expression);
                    self.update.push(format!(
                        "\u{0275}\u{0275}attribute('{}', {compiled});",
                        name,
                    ));
                    self.var_count += 1;
                }
                _ => {}
            }
        }
    }
}

/// Check if an expression has any Angular pipe segments.
fn extract_all_pipe_segments(expr: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let chars: Vec<char> = expr.trim().chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '\'' | '"' | '`' => {
                let quote = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            }
            '|' => {
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    i += 2;
                    continue;
                }
                // Possible pipe — check if followed by identifier
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let name_start = j;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j > name_start {
                    let name: String = chars[name_start..j].iter().collect();
                    segments.push(name);
                }
                i = j;
            }
            _ => {
                i += 1;
            }
        }
    }

    segments
}

/// Split a top-level pipe from an expression: `baseExpr | pipeName` → `(baseExpr, pipeName)`.
///
/// Only splits on `|` at parenthesis depth 0 (not inside parens/brackets).
fn split_top_level_pipe(expr: &str) -> Option<(String, String)> {
    let chars: Vec<char> = expr.chars().collect();
    let mut depth = 0i32;
    let mut last_pipe_pos = None;

    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '(' | '[' => {
                depth += 1;
                i += 1;
            }
            ')' | ']' => {
                depth -= 1;
                i += 1;
            }
            '\'' | '"' | '`' => {
                let quote = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            }
            '|' if depth == 0 => {
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    i += 2;
                    continue;
                }
                last_pipe_pos = Some(i);
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let pos = last_pipe_pos?;
    let base = expr[..pos].trim().to_string();
    let rest = expr[pos + 1..].trim();

    // Extract pipe name (first identifier in rest)
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();

    if name.is_empty() {
        return None;
    }

    Some((base, name))
}

/// Split a top-level pipe from an expression, including pipe arguments.
///
/// Returns `(base_expression, pipe_name, vec_of_args)`.
/// Pipe arguments are separated by `:` after the pipe name.
fn split_top_level_pipe_with_args(expr: &str) -> Option<(String, String, Vec<String>)> {
    let (base, name) = split_top_level_pipe(expr)?;

    // Find where the pipe name ends in the original expression
    let pipe_pos = base.len(); // position of `|`
    let rest = expr[pipe_pos + 1..].trim();
    let after_name = &rest[name.len()..];

    // Parse colon-separated arguments
    let mut args = Vec::new();
    let mut remaining = after_name.trim();
    while remaining.starts_with(':') {
        remaining = remaining[1..].trim();
        // Extract the argument (up to next `:` at depth 0, or end)
        let mut depth = 0i32;
        let mut end = 0;
        let chars: Vec<char> = remaining.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '(' | '[' => {
                    depth += 1;
                    i += 1;
                }
                ')' | ']' => {
                    depth -= 1;
                    i += 1;
                }
                '\'' | '"' | '`' => {
                    let q = chars[i];
                    i += 1;
                    while i < chars.len() && chars[i] != q {
                        if chars[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i < chars.len() {
                        i += 1;
                    }
                }
                ':' if depth == 0 => {
                    break;
                }
                _ => {
                    i += 1;
                }
            }
            end = i;
        }
        let arg = remaining[..end].trim().to_string();
        if !arg.is_empty() {
            args.push(arg);
        }
        remaining = &remaining[end..];
    }

    Some((base, name, args))
}

/// Replace `(expr | pipeName)` sub-expressions with compiled pipe calls.
///
/// Scans for parenthesized expressions containing a single `|` pipe operator
/// and replaces them with the compiled `ɵɵpipeBind1(...)` call.
fn replace_nested_pipe_parens(expr: &str, gen: &mut IvyCodegen) -> String {
    let chars: Vec<char> = expr.chars().collect();
    let mut result = String::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '(' {
            // Find matching close paren
            let start = i;
            let mut depth = 1;
            let mut j = i + 1;
            while j < chars.len() && depth > 0 {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    '\'' | '"' | '`' => {
                        let q = chars[j];
                        j += 1;
                        while j < chars.len() && chars[j] != q {
                            if chars[j] == '\\' {
                                j += 1;
                            }
                            j += 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            // chars[start] = '(', chars[j-1] = ')'
            let inner: String = chars[start + 1..j - 1].iter().collect();

            // Check if the inner expression has a pipe (with optional arguments)
            if let Some((base, pipe_name, args)) = split_top_level_pipe_with_args(&inner) {
                // Compile the base expression recursively
                let compiled_base = replace_nested_pipe_parens(&base, gen);
                let compiled_base = ctx_expr_with_locals(&compiled_base, &gen.local_vars);

                let pipe_slot = gen.slot_index;
                gen.slot_index += 1;
                // Same pipe-offset counter as replace_pipes_in_expr /
                // wrap_with_pipes — keep all three paths consistent so
                // rewrite_pipe_offsets shifts every pipe slot uniformly.
                let pipe_var_slot = gen.pipe_var_offset;
                let slots = 2 + args.len() as u32;
                gen.pipe_var_offset += slots;
                gen.var_count += slots;
                gen.ivy_imports.insert("\u{0275}\u{0275}pipe".to_string());

                let bind_fn = match args.len() {
                    0 => "\u{0275}\u{0275}pipeBind1",
                    1 => "\u{0275}\u{0275}pipeBind2",
                    2 => "\u{0275}\u{0275}pipeBind3",
                    _ => "\u{0275}\u{0275}pipeBindV",
                };
                gen.ivy_imports.insert(bind_fn.to_string());
                gen.creation
                    .push(format!("\u{0275}\u{0275}pipe({pipe_slot}, '{pipe_name}');"));
                if args.is_empty() {
                    result.push_str(&format!(
                        "{bind_fn}({pipe_slot}, {pipe_var_slot}, {compiled_base})"
                    ));
                } else {
                    let compiled_args: Vec<String> = args
                        .iter()
                        .map(|a| compile_pipe_arg(a, &gen.local_vars))
                        .collect();
                    result.push_str(&format!(
                        "{bind_fn}({pipe_slot}, {pipe_var_slot}, {compiled_base}, {})",
                        compiled_args.join(", ")
                    ));
                }
            } else {
                // No pipe inside — recurse on inner, keep parens
                let compiled_inner = replace_nested_pipe_parens(&inner, gen);
                result.push('(');
                result.push_str(&compiled_inner);
                result.push(')');
            }
            i = j;
        } else if chars[i] == '\'' || chars[i] == '"' || chars[i] == '`' {
            // Copy string literals verbatim
            let q = chars[i];
            result.push(q);
            i += 1;
            while i < chars.len() && chars[i] != q {
                if chars[i] == '\\' {
                    result.push(chars[i]);
                    i += 1;
                    if i < chars.len() {
                        result.push(chars[i]);
                        i += 1;
                    }
                } else {
                    result.push(chars[i]);
                    i += 1;
                }
            }
            if i < chars.len() {
                result.push(chars[i]);
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Wrap a template expression with `ctx.` if it's a simple property path.
///
/// Simple paths like `title` or `foo.bar` become `ctx.title` / `ctx.foo.bar`.
/// Complex expressions like `'text' + prop` or `fn()` are left as-is.
/// Compile an Angular template expression to JavaScript by adding `ctx.` prefixes
/// to component property references and stripping TypeScript non-null assertions.
///
/// Uses oxc to parse the expression AST and walk it, ensuring all standalone
/// identifiers (not member properties, not builtins) get the `ctx.` prefix.
fn ctx_expr(expr: &str) -> String {
    ctx_expr_with_locals(expr, &BTreeSet::new())
}

/// Compile a single pipe-call argument to JavaScript.
///
/// Object-literal arguments (e.g. `{ count: tier.count }`) are wrapped in
/// parentheses so oxc parses them as expressions rather than block statements,
/// then unwrapped again. Both forms preserve template-local variables from the
/// enclosing `@for` / `@let` scope so they don't get a spurious `ctx.` prefix.
fn compile_pipe_arg(arg: &str, locals: &BTreeSet<String>) -> String {
    let trimmed = arg.trim();
    if trimmed.starts_with('{') {
        let wrapped = format!("({trimmed})");
        let result = ctx_expr_with_locals(&wrapped, locals);
        result
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or(&result)
            .to_string()
    } else {
        ctx_expr_with_locals(arg, locals)
    }
}

/// Rewrite an expression by adding `ctx.` prefix to top-level identifiers,
/// except for builtins and local variables (like `@let` declarations).
fn ctx_expr_with_locals(expr: &str, locals: &BTreeSet<String>) -> String {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Fast path for simple property paths (skip if it's a local var)
    if is_simple_property_path(trimmed) {
        if locals.contains(trimmed)
            || trimmed.contains('.') && locals.contains(trimmed.split('.').next().unwrap_or(""))
        {
            return trimmed.to_string();
        }
        return format!("ctx.{trimmed}");
    }

    // Parse expression with oxc for proper AST-based rewriting
    let wrapper = format!("var __expr = {trimmed};");
    let alloc = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&alloc, &wrapper, oxc_span::SourceType::tsx()).parse();

    if !parsed.errors.is_empty() || parsed.panicked {
        // If parsing fails, fall back to simple heuristic
        return trimmed.to_string();
    }

    // Extract the initializer expression
    let init_expr = match parsed.program.body.first() {
        Some(oxc_ast::ast::Statement::VariableDeclaration(decl)) => {
            decl.declarations.first().and_then(|d| d.init.as_ref())
        }
        _ => None,
    };

    let init_expr = match init_expr {
        Some(e) => e,
        None => return trimmed.to_string(),
    };

    // Collect positions of identifiers that need `ctx.` prefix and `!` to remove
    let mut ctx_inserts: Vec<u32> = Vec::new();
    let mut remove_ranges: Vec<(u32, u32)> = Vec::new();
    collect_ctx_rewrites(
        init_expr,
        &mut ctx_inserts,
        &mut remove_ranges,
        false,
        locals,
    );

    // Map wrapper byte offsets back to expression byte offsets
    let expr_offset = "var __expr = ".len() as u32;

    // Build a unified list of edits (all with byte offsets into `trimmed`)
    // sorted by position descending so we can apply them back-to-front
    // without invalidating earlier positions.
    enum Edit {
        Insert(usize),        // insert "ctx." at this byte offset
        Remove(usize, usize), // remove bytes [start..end)
    }

    let mut edits: Vec<(usize, Edit)> = Vec::new();

    for off in &ctx_inserts {
        let pos = (*off - expr_offset) as usize;
        edits.push((pos, Edit::Insert(pos)));
    }
    for (s, e) in &remove_ranges {
        let start = (*s - expr_offset) as usize;
        let end = (*e - expr_offset) as usize;
        edits.push((start, Edit::Remove(start, end)));
    }

    // Sort by position descending; removals before insertions at same position
    edits.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            // Removals first at same position
            let a_is_remove = matches!(a.1, Edit::Remove(..));
            let b_is_remove = matches!(b.1, Edit::Remove(..));
            b_is_remove.cmp(&a_is_remove)
        })
    });

    // Deduplicate insertions at the same position
    edits.dedup_by(|a, b| matches!((&a.1, &b.1), (Edit::Insert(_), Edit::Insert(_))) && a.0 == b.0);

    let mut result = trimmed.to_string();
    for (_pos, edit) in &edits {
        match edit {
            Edit::Insert(off) => {
                if *off <= result.len() && result.is_char_boundary(*off) {
                    result.insert_str(*off, "ctx.");
                }
            }
            Edit::Remove(s, e) => {
                if *s <= result.len()
                    && *e <= result.len()
                    && result.is_char_boundary(*s)
                    && result.is_char_boundary(*e)
                {
                    result.replace_range(*s..*e, "");
                }
            }
        }
    }

    result
}

/// Built-in `@for` block locals exposed on the embedded view's context. Each
/// is read via `_ctx.<name>` when the loop body references it. Order is fixed
/// to keep the emitted prelude byte-stable across runs.
const FOR_BUILTIN_LOCALS: &[&str] = &["$index", "$count", "$first", "$last", "$even", "$odd"];

/// Check whether `needle` appears as a whole-word identifier in `haystack`.
/// "Whole-word" treats ASCII alphanumerics, `_`, and `$` as identifier
/// continuation — matching JavaScript's identifier rules for the names this
/// function is used with (loop item aliases and `@for` built-ins). A match
/// inside a string literal still counts as a hit; the haystack here is the
/// already-compiled binding code, not arbitrary user prose.
fn identifier_used_in(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let start = search_from + rel;
        let end = start + needle.len();
        let prev_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .is_some_and(is_js_ident_continue);
        let next_ok = end == haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .is_some_and(is_js_ident_continue);
        if prev_ok && next_ok {
            return true;
        }
        search_from = start + 1;
    }
    false
}

fn is_js_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Detect a reference to the parent component's `ctx` (rather than `_ctx`,
/// `_outer_ctx`, etc.) anywhere in the joined update instructions. Used by
/// `@for` body emission to decide whether to call `ɵɵnextContext`.
fn update_references_parent_ctx(updates: &[String]) -> bool {
    updates.iter().any(|s| has_bare_ctx_ref(s))
}

fn has_bare_ctx_ref(s: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel) = s[search_from..].find("ctx.") {
        let start = search_from + rel;
        let prev_ok = start == 0
            || !s[..start]
                .chars()
                .next_back()
                .is_some_and(is_js_ident_continue);
        if prev_ok {
            return true;
        }
        search_from = start + 1;
    }
    false
}

/// Check if a string is a simple property path (e.g. `foo`, `foo.bar`, `$data`).
fn is_simple_property_path(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '$')
}

/// Recursively collect identifier positions that need `ctx.` prefix and
/// TypeScript non-null assertion `!` positions to remove.
fn collect_ctx_rewrites(
    expr: &oxc_ast::ast::Expression<'_>,
    ctx_inserts: &mut Vec<u32>,
    remove_ranges: &mut Vec<(u32, u32)>,
    is_member_property: bool,
    locals: &BTreeSet<String>,
) {
    use oxc_ast::ast::*;
    use oxc_span::GetSpan;

    let is_local = |name: &str| -> bool { locals.contains(name) };

    fn is_builtin(name: &str) -> bool {
        // Angular runtime symbols (ɵɵ-prefixed) are not component properties
        if name.starts_with('\u{0275}') {
            return true;
        }
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
        Expression::Identifier(id)
            if !is_member_property && !is_builtin(&id.name) && !is_local(&id.name) =>
        {
            ctx_inserts.push(id.span.start);
        }
        Expression::CallExpression(call) => {
            collect_ctx_rewrites(&call.callee, ctx_inserts, remove_ranges, false, locals);
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    collect_ctx_rewrites(
                        &spread.argument,
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
                } else {
                    collect_ctx_rewrites(
                        arg.to_expression(),
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
                }
            }
        }
        Expression::StaticMemberExpression(member) => {
            collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::ComputedMemberExpression(member) => {
            collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false, locals);
            collect_ctx_rewrites(
                &member.expression,
                ctx_inserts,
                remove_ranges,
                false,
                locals,
            );
        }
        Expression::UnaryExpression(unary) => {
            collect_ctx_rewrites(&unary.argument, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::BinaryExpression(binary) => {
            collect_ctx_rewrites(&binary.left, ctx_inserts, remove_ranges, false, locals);
            collect_ctx_rewrites(&binary.right, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::LogicalExpression(logical) => {
            collect_ctx_rewrites(&logical.left, ctx_inserts, remove_ranges, false, locals);
            collect_ctx_rewrites(&logical.right, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::ConditionalExpression(cond) => {
            collect_ctx_rewrites(&cond.test, ctx_inserts, remove_ranges, false, locals);
            collect_ctx_rewrites(&cond.consequent, ctx_inserts, remove_ranges, false, locals);
            collect_ctx_rewrites(&cond.alternate, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_ctx_rewrites(&paren.expression, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::AssignmentExpression(assign) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &assign.left {
                if !is_builtin(&id.name) {
                    ctx_inserts.push(id.span.start);
                }
            }
            collect_ctx_rewrites(&assign.right, ctx_inserts, remove_ranges, false, locals);
        }
        Expression::TSNonNullExpression(non_null) => {
            collect_ctx_rewrites(
                &non_null.expression,
                ctx_inserts,
                remove_ranges,
                is_member_property,
                locals,
            );
            let inner_end = non_null.expression.span().end;
            let outer_end = non_null.span.end;
            if outer_end > inner_end {
                remove_ranges.push((inner_end, outer_end));
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    collect_ctx_rewrites(&p.value, ctx_inserts, remove_ranges, false, locals);
                }
            }
        }
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let ArrayExpressionElement::SpreadElement(spread) = elem {
                    collect_ctx_rewrites(
                        &spread.argument,
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
                } else if !elem.is_elision() {
                    collect_ctx_rewrites(
                        elem.to_expression(),
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
                }
            }
        }
        Expression::TemplateLiteral(tpl) => {
            for expr in &tpl.expressions {
                collect_ctx_rewrites(expr, ctx_inserts, remove_ranges, false, locals);
            }
        }
        Expression::ChainExpression(chain) => {
            // Optional chaining (a?.b, a?.(), a?.[b]) — recurse into the inner expression
            match &chain.expression {
                ChainElement::CallExpression(call) => {
                    collect_ctx_rewrites(&call.callee, ctx_inserts, remove_ranges, false, locals);
                    for arg in &call.arguments {
                        if let Argument::SpreadElement(spread) = arg {
                            collect_ctx_rewrites(
                                &spread.argument,
                                ctx_inserts,
                                remove_ranges,
                                false,
                                locals,
                            );
                        } else {
                            collect_ctx_rewrites(
                                arg.to_expression(),
                                ctx_inserts,
                                remove_ranges,
                                false,
                                locals,
                            );
                        }
                    }
                }
                ChainElement::StaticMemberExpression(member) => {
                    collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false, locals);
                }
                ChainElement::ComputedMemberExpression(member) => {
                    collect_ctx_rewrites(&member.object, ctx_inserts, remove_ranges, false, locals);
                    collect_ctx_rewrites(
                        &member.expression,
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Build a conditional expression for @if chains using absolute slot indices.
fn build_conditional_expr(
    condition: &str,
    if_slot: u32,
    else_ifs: &[(String, String, u32)],
    else_info: &Option<(String, u32)>,
    locals: &BTreeSet<String>,
) -> String {
    let mut expr = format!(
        "{} ? {} : ",
        ctx_expr_with_locals(condition, locals),
        if_slot
    );

    for (cond, _fn_name, slot) in else_ifs {
        expr.push_str(&format!(
            "{} ? {} : ",
            ctx_expr_with_locals(cond, locals),
            slot
        ));
    }

    if let Some((_fn, slot)) = else_info {
        expr.push_str(&format!("{}", slot));
    } else {
        expr.push_str("-1");
    }

    expr
}

/// Format static attributes as an array expression.
/// Pre-scope component CSS with `%COMP%` placeholders for Angular's emulated ViewEncapsulation.
///
/// Transforms:
/// - `.class { ... }` → `.class[_ngcontent-%COMP%] { ... }`
/// - `:host { ... }` → `[_nghost-%COMP%] { ... }`
/// - `:host-context(X) .class { ... }` → `X[_nghost-%COMP%] .class[_ngcontent-%COMP%], X [_nghost-%COMP%] .class[_ngcontent-%COMP%] { ... }`
fn scope_component_styles(styles_src: &str) -> String {
    // `styles_src` is a JavaScript array expression of string / template
    // literals — `[\`css\`]`, `['css']`, or multi-element forms like
    // `[\`a\`, \`b\`]`. Walk the source and scope each literal's *content*
    // independently, preserving everything outside the literals (brackets,
    // commas, whitespace) verbatim. The prior implementation used
    // `find('`')..rfind('`')` and treated every character in between as CSS,
    // which swept intermediate `\`,\`` boundaries into the selector walker
    // and emitted the trailing `[_ngcontent-%COMP%]` outside the template
    // literal — producing invalid TypeScript that panics oxc (GH #81).
    let mut result = String::with_capacity(styles_src.len());
    let mut iter = styles_src.char_indices().peekable();
    while let Some((i, ch)) = iter.next() {
        if matches!(ch, '`' | '"' | '\'') {
            let delim = ch;
            let content_start = i + ch.len_utf8();
            let mut content_end: Option<usize> = None;
            while let Some((j, c)) = iter.next() {
                if c == '\\' {
                    // Preserve the escape — skip whatever follows the backslash.
                    let _ = iter.next();
                    continue;
                }
                if c == delim {
                    content_end = Some(j);
                    break;
                }
            }
            match content_end {
                Some(end) => {
                    let content = &styles_src[content_start..end];
                    result.push(delim);
                    // Skip scoping if a template literal has an interpolation
                    // — `${...}` spans JS, not CSS, and a naive brace walk
                    // would mangle it.
                    if delim == '`' && content.contains("${") {
                        result.push_str(content);
                    } else {
                        result.push_str(&scope_single_css(content));
                    }
                    result.push(delim);
                }
                None => {
                    // Unterminated literal (malformed input). Bail out —
                    // emit the remainder verbatim so we do no further harm.
                    result.push_str(&styles_src[i..]);
                    return result;
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Scope a single CSS string with `%COMP%` placeholders. Extracted from
/// [`scope_component_styles`] so we can apply the transform per array
/// element instead of across the entire array source.
fn scope_single_css(css: &str) -> String {
    let content_attr = "[_ngcontent-%COMP%]";
    let host_attr = "[_nghost-%COMP%]";

    let mut result = String::new();
    let mut selector = String::new();
    let mut in_block = false;
    let mut brace_depth = 0;

    for ch in css.chars() {
        if in_block {
            result.push(ch);
            if ch == '{' {
                brace_depth += 1;
            }
            if ch == '}' {
                brace_depth -= 1;
                if brace_depth == 0 {
                    in_block = false;
                }
            }
        } else if ch == '{' {
            let sel = selector.trim().to_string();
            if sel.is_empty() {
                result.push('{');
                in_block = true;
                brace_depth = 1;
                selector.clear();
                continue;
            }

            if sel.contains(":host-context(") {
                let after_hc = sel
                    .find(":host-context(")
                    .map(|i| &sel[i + ":host-context(".len()..])
                    .unwrap_or("");
                let close = after_hc.find(')').unwrap_or(after_hc.len());
                let context = after_hc[..close].trim();
                let rest = after_hc[close + 1..].trim();
                let scoped_rest = if rest.is_empty() {
                    String::new()
                } else {
                    scope_simple_selector(rest, content_attr)
                };
                result.push_str(&format!(
                    "{context}{host_attr} {scoped_rest}, {context} {host_attr} {scoped_rest}"
                ));
            } else if let Some(stripped) = sel.strip_prefix(":host") {
                let rest = stripped.trim();
                if rest.is_empty() {
                    result.push_str(host_attr);
                } else if rest.starts_with('(') {
                    let inner = rest.trim_start_matches('(').trim_end_matches(')').trim();
                    result.push_str(&format!("{inner}{host_attr}"));
                } else {
                    result.push_str(&format!("{host_attr} {rest}"));
                }
            } else {
                let parts: Vec<&str> = sel.split(',').collect();
                let scoped: Vec<String> = parts
                    .iter()
                    .map(|p| scope_simple_selector(p.trim(), content_attr))
                    .collect();
                result.push_str(&scoped.join(", "));
            }

            result.push('{');
            in_block = true;
            brace_depth = 1;
            selector.clear();
        } else {
            selector.push(ch);
        }
    }

    // Preserve any trailing whitespace / comments that never made it into a
    // block so round-tripping of a pure-whitespace CSS string is a no-op.
    if !selector.is_empty() && selector.trim().is_empty() {
        result.push_str(&selector);
    }

    result
}

/// Append `[_ngcontent-%COMP%]` to the last element in a simple CSS selector.
fn scope_simple_selector(selector: &str, content_attr: &str) -> String {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Split on spaces, append attr to last part
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let mut result: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|p| p.to_string())
        .collect();
    let last = parts.last().unwrap();
    result.push(format!("{last}{content_attr}"));
    result.join(" ")
}

/// Get the tag name and consts index of the first root element in a template block.
/// Returns (tag, consts_index) for the conditional host element.
fn get_root_element_info(
    children: &[TemplateNode],
    gen: &mut IvyCodegen,
) -> (Option<String>, Option<usize>) {
    for child in children {
        match child {
            TemplateNode::Element(el) => {
                let tag = el.tag.clone();
                // Extract static attributes for the consts array — include
                // value-less attributes with empty string for directive matching.
                let static_attrs: Vec<(&str, &str)> = el
                    .attributes
                    .iter()
                    .filter_map(|a| match a {
                        crate::ast::TemplateAttribute::Static {
                            name,
                            value: Some(v),
                        } => Some((name.as_str(), v.as_str())),
                        crate::ast::TemplateAttribute::Static { name, value: None } => {
                            Some((name.as_str(), ""))
                        }
                        _ => None,
                    })
                    .collect();
                let consts_idx = if !static_attrs.is_empty() {
                    Some(gen.register_const(&static_attrs))
                } else {
                    None
                };
                return (Some(tag), consts_idx);
            }
            TemplateNode::Text(t) if t.value.trim().is_empty() => continue,
            _ => return (None, None),
        }
    }
    (None, None)
}

/// Format a consts array entry with static attributes and optional binding markers.
/// Angular's AttributeMarker.Bindings (3) marks binding attribute names for directive matching.
fn format_static_attrs(attrs: &[(&str, &str)]) -> String {
    format_attrs_with_bindings(attrs, &[])
}

/// Format a consts array entry with both static attributes and binding markers.
///
/// Uses Angular's AttributeMarker format:
/// - `1` (Classes) + individual class names for `class` attributes
/// - Regular key-value pairs for other attributes
/// - `3` (Bindings) + binding names for directive input matching
fn format_attrs_with_bindings(attrs: &[(&str, &str)], binding_names: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut class_names: Vec<String> = Vec::new();

    for (k, v) in attrs {
        if *k == "class" && !v.is_empty() {
            // Split class value into individual names for AttributeMarker.Classes
            for cls in v.split_whitespace() {
                class_names.push(format!("'{}'", escape_js_string(cls)));
            }
        } else {
            parts.push(format!("'{}'", escape_js_string(k)));
            parts.push(format!("'{}'", escape_js_string(v)));
        }
    }

    // Insert AttributeMarker.Classes (1) + class names
    if !class_names.is_empty() {
        parts.push("1".to_string());
        parts.extend(class_names);
    }

    // Append AttributeMarker.Bindings (3) + binding attribute names
    if !binding_names.is_empty() {
        parts.push("3".to_string());
        for name in binding_names {
            parts.push(format!("'{}'", escape_js_string(name)));
        }
    }
    format!("[{}]", parts.join(", "))
}

/// Compile an Angular event handler expression.
///
/// Handles multi-statement handlers like `$event.stopPropagation(); doSomething()`
/// by splitting on `;`, applying `ctx.` to each statement, and adding `return` to
/// the last statement.
fn compile_event_handler(handler: &str, locals: &BTreeSet<String>) -> String {
    let trimmed = handler.trim();
    // Split on semicolons (respecting strings and parens)
    let statements: Vec<&str> = trimmed
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if statements.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    for (i, stmt) in statements.iter().enumerate() {
        let compiled = ctx_expr_with_locals(stmt, locals);
        if i == statements.len() - 1 {
            parts.push(format!("return {compiled};"));
        } else {
            parts.push(format!("{compiled};"));
        }
    }

    parts.join(" ")
}

/// Build the module-scope dependency resolver function for an `@defer`
/// block.
///
/// Emits a function that returns an array of `import()` promises — one per
/// custom element tag (tags containing a `-`) found inside the block. The
/// helper keeps the resolver inert when the block contains only HTML tags:
/// an empty array is a valid result that tells the runtime there is
/// nothing to load asynchronously.
///
/// Real-world components are wired by the bundler's import-scanner picking
/// up these `import(...)` calls as split points; resolving each call to a
/// concrete module source is the author's responsibility for now — the
/// specifier is derived from the tag (e.g. `<my-comp />` →
/// `'./my-comp'`). Component authors who need a different path can
/// re-author the helper by hand after compilation.
fn build_defer_deps_fn(name: &str, children: &[TemplateNode]) -> String {
    let mut tags: Vec<String> = Vec::new();
    collect_custom_tags(children, &mut tags);
    tags.sort();
    tags.dedup();

    let mut code = format!("function {name}() {{\n  return [");
    if tags.is_empty() {
        code.push(']');
    } else {
        code.push('\n');
        for (i, tag) in tags.iter().enumerate() {
            let symbol = kebab_to_pascal(tag);
            code.push_str(&format!("    import('./{tag}').then(m => m.{symbol})"));
            if i + 1 < tags.len() {
                code.push(',');
            }
            code.push('\n');
        }
        code.push_str("  ]");
    }
    code.push_str(";\n}");
    code
}

/// Walk a subtree collecting the names of custom-element tags (any tag
/// whose name contains `-`, the HTML signal for a web component or
/// Angular component selector).
fn collect_custom_tags(nodes: &[TemplateNode], out: &mut Vec<String>) {
    for node in nodes {
        match node {
            TemplateNode::Element(el) => {
                if el.tag.contains('-') {
                    out.push(el.tag.clone());
                }
                collect_custom_tags(&el.children, out);
            }
            TemplateNode::IfBlock(b) => {
                collect_custom_tags(&b.children, out);
                for branch in &b.else_if_branches {
                    collect_custom_tags(&branch.children, out);
                }
                if let Some(else_c) = &b.else_branch {
                    collect_custom_tags(else_c, out);
                }
            }
            TemplateNode::ForBlock(b) => {
                collect_custom_tags(&b.children, out);
                if let Some(empty) = &b.empty_children {
                    collect_custom_tags(empty, out);
                }
            }
            TemplateNode::SwitchBlock(b) => {
                for c in &b.cases {
                    collect_custom_tags(&c.children, out);
                }
                if let Some(d) = &b.default_branch {
                    collect_custom_tags(d, out);
                }
            }
            TemplateNode::DeferBlock(b) => {
                collect_custom_tags(&b.children, out);
                if let Some(p) = &b.placeholder {
                    collect_custom_tags(p, out);
                }
                if let Some(l) = &b.loading {
                    collect_custom_tags(l, out);
                }
                if let Some(e) = &b.error {
                    collect_custom_tags(e, out);
                }
            }
            _ => {}
        }
    }
}

/// Convert a kebab-case tag name to a PascalCase identifier.
/// `my-cool-widget` → `MyCoolWidget`.
fn kebab_to_pascal(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    let mut capitalize = true;
    for ch in tag.chars() {
        if ch == '-' {
            capitalize = true;
        } else if capitalize {
            out.extend(ch.to_uppercase());
            capitalize = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Parse an Angular duration string to milliseconds.
///
/// Accepts `<number>ms` or `<number>s`; a bare number is treated as ms.
/// Unknown suffixes fall back to the raw string as the argument (so the
/// runtime or a later lint can flag it).
fn parse_duration_to_ms(duration: &str) -> String {
    let trimmed = duration.trim();
    if let Some(prefix) = trimmed.strip_suffix("ms") {
        if let Ok(n) = prefix.trim().parse::<u64>() {
            return n.to_string();
        }
    }
    if let Some(prefix) = trimmed.strip_suffix('s') {
        if let Ok(n) = prefix.trim().parse::<u64>() {
            return (n * 1000).to_string();
        }
    }
    if trimmed.parse::<u64>().is_ok() {
        return trimmed.to_string();
    }
    format!("'{}'", escape_js_string(trimmed))
}

/// Escape a string for use inside a single-quoted JavaScript string literal.
/// Count sequential binding slots in the update block.
/// These are consumed by nextBindingIndex() at runtime.
fn count_sequential_bindings(update: &[String]) -> u32 {
    let mut count: u32 = 0;
    for instr in update {
        // textInterpolate, textInterpolate1 = 1 binding each
        if instr.contains("textInterpolate") {
            count += 1;
        }
        // property, attribute = 1 binding each
        if instr.contains("\u{0275}\u{0275}property(")
            || instr.contains("\u{0275}\u{0275}attribute(")
        {
            count += 1;
        }
        // conditional = 1 binding
        if instr.contains("\u{0275}\u{0275}conditional(") {
            count += 1;
        }
        // classProp, styleProp, classMap, styleMap = 2 binding slots each
        // (Angular's styling system uses incrementBindingIndex(2))
        if instr.contains("\u{0275}\u{0275}classProp(")
            || instr.contains("\u{0275}\u{0275}styleProp(")
            || instr.contains("\u{0275}\u{0275}classMap(")
            || instr.contains("\u{0275}\u{0275}styleMap(")
        {
            count += 2;
        }
        // repeater = 1 binding
        if instr.contains("\u{0275}\u{0275}repeater(") {
            count += 1;
        }
        // deferWhen / deferPrefetchWhen = 1 binding each
        if instr.contains("\u{0275}\u{0275}deferWhen(")
            || instr.contains("\u{0275}\u{0275}deferPrefetchWhen(")
        {
            count += 1;
        }
    }
    count
}

/// Count total pipe binding slots used in the update block.
fn count_pipe_binding_slots(update: &[String]) -> u32 {
    let mut total: u32 = 0;
    for instr in update {
        // pipeBind1 = 2 slots, pipeBind2 = 3, pipeBind3 = 4
        for (name, slots) in [("pipeBind1", 2), ("pipeBind2", 3), ("pipeBind3", 4)] {
            let count = instr.matches(name).count() as u32;
            total += count * slots;
        }
    }
    total
}

/// Rewrite pipe offset parameters in an update instruction.
/// Adds `seq_bindings` to each pipe's second parameter (the offset).
fn rewrite_pipe_offsets(instr: &str, seq_bindings: u32) -> String {
    let mut result = instr.to_string();
    // Find all pipeBind*( patterns and shift their offset parameter
    for prefix in [
        "\u{0275}\u{0275}pipeBind1(",
        "\u{0275}\u{0275}pipeBind2(",
        "\u{0275}\u{0275}pipeBind3(",
        "\u{0275}\u{0275}pipeBindV(",
    ] {
        let mut search_from = 0;
        while let Some(start) = result[search_from..].find(prefix) {
            let abs_start = search_from + start + prefix.len();
            // Skip first arg (pipe slot)
            let comma1 = match result[abs_start..].find(',') {
                Some(i) => abs_start + i + 1,
                None => break,
            };
            // Find the second arg (offset) — it's a number
            let trimmed = result[comma1..].trim_start();
            let offset_start = comma1 + (result[comma1..].len() - trimmed.len());
            let offset_end = result[offset_start..]
                .find(|c: char| !c.is_ascii_digit())
                .map(|i| offset_start + i)
                .unwrap_or(result.len());
            if offset_start < offset_end {
                if let Ok(current_offset) = result[offset_start..offset_end].parse::<u32>() {
                    let new_offset = current_offset + seq_bindings;
                    result = format!(
                        "{}{}{}",
                        &result[..offset_start],
                        new_offset,
                        &result[offset_end..]
                    );
                }
            }
            search_from = offset_start + 1;
        }
    }
    result
}

fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Decode common HTML character entities to their Unicode equivalents.
fn decode_html_entities(s: &str) -> String {
    let mut result = s.to_string();
    // Named entities (most common ones used in Angular templates)
    let entities = [
        ("&times;", "\u{00D7}"),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&nbsp;", "\u{00A0}"),
        ("&laquo;", "\u{00AB}"),
        ("&raquo;", "\u{00BB}"),
        ("&mdash;", "\u{2014}"),
        ("&ndash;", "\u{2013}"),
        ("&hellip;", "\u{2026}"),
        ("&copy;", "\u{00A9}"),
        ("&reg;", "\u{00AE}"),
        ("&trade;", "\u{2122}"),
        ("&bull;", "\u{2022}"),
        ("&larr;", "\u{2190}"),
        ("&rarr;", "\u{2192}"),
        ("&uarr;", "\u{2191}"),
        ("&darr;", "\u{2193}"),
        ("&check;", "\u{2713}"),
        ("&cross;", "\u{2717}"),
    ];
    for (entity, ch) in entities {
        result = result.replace(entity, ch);
    }
    // Numeric entities: &#NNN; and &#xHHH;
    while let Some(start) = result.find("&#") {
        let rest = &result[start + 2..];
        if let Some(end) = rest.find(';') {
            let num_str = &rest[..end];
            let decoded = if let Some(hex) = num_str.strip_prefix('x') {
                u32::from_str_radix(hex, 16).ok()
            } else {
                num_str.parse::<u32>().ok()
            };
            if let Some(cp) = decoded.and_then(char::from_u32) {
                let mut buf = [0u8; 4];
                let replacement = cp.encode_utf8(&mut buf);
                result = format!(
                    "{}{}{}",
                    &result[..start],
                    replacement,
                    &result[start + 2 + end + 1..]
                );
                continue;
            }
        }
        break; // malformed entity — stop processing
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::ExtractedComponent;

    fn test_component() -> ExtractedComponent {
        ExtractedComponent {
            class_name: "TestComponent".to_string(),
            selector: "app-test".to_string(),
            template: Some("<h1>Hello</h1>".to_string()),
            template_url: None,
            standalone: true,
            imports_source: None,
            imports_identifiers: Vec::new(),
            decorator_span: (0, 0),
            class_body_start: 0,
            export_keyword_start: None,
            class_keyword_start: 0,
            angular_core_import_span: None,
            other_angular_core_imports: Vec::new(),
            styles_source: None,
            inline_styles: Vec::new(),
            style_urls: Vec::new(),
            input_properties: Vec::new(),
            host_listeners: Vec::new(),
            host_bindings: Vec::new(),
            animations_source: None,
            host_directives_source: None,
            signal_inputs: Vec::new(),
            signal_outputs: Vec::new(),
            signal_models: Vec::new(),
            signal_queries: Vec::new(),
            change_detection: None,
        }
    }

    /// Parse a template string and generate Ivy output, returning the
    /// defineComponent source (static_fields[0]). Panics on parse failure;
    /// tests should use literals that are known-valid.
    fn compile_template(src: &str) -> IvyOutput {
        use crate::parser::parse_template;
        use std::path::PathBuf;
        let nodes = parse_template(src, &PathBuf::from("test.html")).expect("parse");
        generate_ivy(&test_component(), &nodes).expect("generate")
    }

    #[test]
    fn test_svg_namespace_emitted_before_svg_element() {
        let output = compile_template("<svg><g><path d='M0 0'/></g></svg>");
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}namespaceSVG"),
            "namespaceSVG should be imported"
        );
        let dc = &output.static_fields[0];
        let ns_idx = dc
            .find("\u{0275}\u{0275}namespaceSVG()")
            .expect("namespaceSVG should be emitted");
        let svg_start = dc
            .find("\u{0275}\u{0275}elementStart(0, 'svg')")
            .expect("elementStart for svg should be emitted");
        assert!(
            ns_idx < svg_start,
            "namespaceSVG must come before elementStart('svg'): {dc}"
        );
        // Descendants inherit SVG — no redundant transitions inside the subtree.
        assert_eq!(
            dc.matches("\u{0275}\u{0275}namespaceSVG()").count(),
            1,
            "only one namespaceSVG transition expected: {dc}"
        );
    }

    #[test]
    fn test_namespace_restores_to_html_after_svg_subtree() {
        let output = compile_template("<svg><path/></svg><div></div>");
        let dc = &output.static_fields[0];
        let svg_idx = dc
            .find("\u{0275}\u{0275}namespaceSVG()")
            .expect("namespaceSVG should be emitted");
        let html_idx = dc
            .find("\u{0275}\u{0275}namespaceHTML()")
            .expect("namespaceHTML transition should be emitted for trailing HTML");
        let div_start = dc.find("'div'").expect("div element should be emitted");
        assert!(
            svg_idx < html_idx && html_idx < div_start,
            "namespaceSVG → namespaceHTML → div expected: {dc}"
        );
    }

    #[test]
    fn test_foreign_object_children_return_to_html() {
        let output = compile_template("<svg><foreignObject><div>x</div></foreignObject></svg>");
        let dc = &output.static_fields[0];
        let svg_ns = dc
            .find("\u{0275}\u{0275}namespaceSVG()")
            .expect("namespaceSVG should be emitted before svg");
        let fo_start = dc
            .find("'foreignObject'")
            .expect("foreignObject elementStart");
        let html_ns = dc
            .find("\u{0275}\u{0275}namespaceHTML()")
            .expect("namespaceHTML should be emitted for foreignObject's HTML children");
        let div_start = dc
            .find("'div'")
            .expect("div elementStart inside foreignObject");
        assert!(
            svg_ns < fo_start,
            "namespaceSVG must precede foreignObject: {dc}"
        );
        assert!(
            fo_start < html_ns && html_ns < div_start,
            "namespaceHTML must be emitted between foreignObject and its div child: {dc}"
        );
    }

    #[test]
    fn test_math_ns_emitted_before_math_element() {
        let output = compile_template("<math><mrow></mrow></math>");
        assert!(
            output
                .ivy_imports
                .contains("\u{0275}\u{0275}namespaceMathML"),
            "namespaceMathML should be imported"
        );
        let dc = &output.static_fields[0];
        let ns = dc
            .find("\u{0275}\u{0275}namespaceMathML()")
            .expect("namespaceMathML should be emitted");
        let math = dc.find("'math'").expect("math elementStart");
        assert!(ns < math, "namespaceMathML must precede math: {dc}");
    }

    #[test]
    fn test_void_element_codegen() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "br".to_string(),
            attributes: Vec::new(),
            children: Vec::new(),
            is_void: true,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        assert!(output.static_fields[0].contains("decls: 1"));
        assert!(output.static_fields[0].contains("vars: 0"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}element"));
    }

    #[test]
    fn test_paired_element_with_text() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "h1".to_string(),
            attributes: Vec::new(),
            children: vec![TemplateNode::Text(TextNode {
                value: "Hello".to_string(),
            })],
            is_void: false,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        assert!(output.static_fields[0].contains("decls: 2"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}elementStart"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}text"));
    }

    #[test]
    fn test_interpolation_codegen() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Interpolation(InterpolationNode {
            expression: "title".to_string(),
            pipes: Vec::new(),
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        assert!(output.static_fields[0].contains("decls: 1"));
        assert!(output.static_fields[0].contains("vars: 1"));
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}textInterpolate(ctx.title);"));
    }

    #[test]
    fn test_event_binding_codegen() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "button".to_string(),
            attributes: vec![TemplateAttribute::Event {
                name: "click".to_string(),
                handler: "onClick()".to_string(),
            }],
            children: vec![TemplateNode::Text(TextNode {
                value: "Click".to_string(),
            })],
            is_void: false,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}listener"));
        assert!(output.static_fields[0].contains("listener"));
    }

    #[test]
    fn test_factory_code() {
        let comp = test_component();
        let nodes = vec![];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        assert!(output.factory_code.contains("TestComponent_Factory"));
        assert!(output.factory_code.contains("new (t || TestComponent)()"));
    }

    #[test]
    fn test_ctx_expr_simple() {
        assert_eq!(ctx_expr("title"), "ctx.title");
        assert_eq!(ctx_expr("foo.bar"), "ctx.foo.bar");
    }

    #[test]
    fn test_ctx_expr_negation() {
        assert_eq!(ctx_expr("!isCollapsed"), "!ctx.isCollapsed");
    }

    #[test]
    fn test_ctx_expr_logical_or() {
        assert_eq!(
            ctx_expr("!isCollapsed || mobileMenu.isMobileMenuOpen()"),
            "!ctx.isCollapsed || ctx.mobileMenu.isMobileMenuOpen()"
        );
    }

    #[test]
    fn test_ctx_expr_method_call() {
        assert_eq!(
            ctx_expr("getBadgeClass(subscription().tier)"),
            "ctx.getBadgeClass(ctx.subscription().tier)"
        );
    }

    #[test]
    fn test_ctx_expr_ternary() {
        assert_eq!(
            ctx_expr("isCollapsed ? 'Expand sidebar' : 'Collapse sidebar'"),
            "ctx.isCollapsed ? 'Expand sidebar' : 'Collapse sidebar'"
        );
    }

    #[test]
    fn test_ctx_expr_ts_non_null_stripped() {
        assert_eq!(ctx_expr("subscription()!.tier"), "ctx.subscription().tier");
    }

    #[test]
    fn test_ctx_expr_object_literal() {
        assert_eq!(ctx_expr("{ exact: true }"), "{ exact: true }");
    }

    #[test]
    fn test_ctx_expr_negation_of_member() {
        assert_eq!(ctx_expr("!auth.token"), "!ctx.auth.token");
    }

    #[test]
    fn test_ctx_expr_style_transform() {
        assert_eq!(
            ctx_expr("isCollapsed ? 'rotate(180deg)' : 'rotate(0deg)'"),
            "ctx.isCollapsed ? 'rotate(180deg)' : 'rotate(0deg)'"
        );
    }

    #[test]
    fn test_extract_all_pipe_segments() {
        let segments = extract_all_pipe_segments("'NAV.UPGRADE' | translate");
        assert_eq!(segments, vec!["translate"]);

        let segments = extract_all_pipe_segments("foo || bar");
        assert!(segments.is_empty(), "|| should not be treated as pipe");

        let segments =
            extract_all_pipe_segments("x === 'a' ? ('B' | translate) : ('C' | translate)");
        assert_eq!(segments, vec!["translate", "translate"]);
    }

    #[test]
    fn test_split_top_level_pipe() {
        assert_eq!(
            split_top_level_pipe("'NAV.UPGRADE' | translate"),
            Some(("'NAV.UPGRADE'".to_string(), "translate".to_string()))
        );
        assert_eq!(split_top_level_pipe("foo || bar"), None,);
        // Pipe inside parens is not top-level
        assert_eq!(split_top_level_pipe("x ? ('A' | translate) : 'B'"), None,);
    }

    #[test]
    fn test_ctx_expr_multiple_non_null_assertions() {
        // Reproduces the pattern that caused the char boundary panic:
        // removing `!` shifts byte offsets for subsequent insertions
        let result = ctx_expr("getBadgeClass(subscription()!.tier, subscription()!.status)");
        assert!(result.contains("ctx.getBadgeClass"));
        assert!(result.contains("ctx.subscription().tier"));
        assert!(result.contains("ctx.subscription().status"));
        assert!(!result.contains('!'));
    }

    #[test]
    fn test_compile_binding_expr_with_nested_pipes() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "div".to_string(),
            attributes: vec![TemplateAttribute::AttrBinding {
                name: "title".to_string(),
                expression:
                    "x === 'free' ? ('NAV.UPGRADE' | translate) : ('NAV.BILLING' | translate)"
                        .to_string(),
            }],
            children: vec![],
            is_void: false,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let dc = &output.static_fields[0];
        // Should have pipeBind1 calls instead of raw | translate
        assert!(
            dc.contains("pipeBind1"),
            "should compile pipes to pipeBind1: {dc}"
        );
        assert!(
            !dc.contains("| translate"),
            "should not have raw pipe syntax: {dc}"
        );
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}pipe"));
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}pipeBind1"));
    }

    #[test]
    fn test_pipe_object_arg_preserves_for_local() {
        // `@for (tier of tiers()) { {{ 'K' | translate: { count: tier.features['portfolios'] } }} }`
        // The pipe's second argument references the @for loop variable `tier`, which
        // must stay unprefixed in the emitted code — NOT `ctx.tier.features[...]`.
        let comp = test_component();
        let nodes = vec![TemplateNode::ForBlock(ForBlockNode {
            item_name: "tier".to_string(),
            iterable: "tiers()".to_string(),
            track_expression: "tier".to_string(),
            children: vec![TemplateNode::Interpolation(InterpolationNode {
                expression: "'PRICING.PORTFOLIO_COUNT'".to_string(),
                pipes: vec![PipeCall {
                    name: "translate".to_string(),
                    args: vec!["{ count: tier.features['portfolios'] }".to_string()],
                }],
            })],
            empty_children: None,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let body = output.child_template_functions.join("\n");
        assert!(
            body.contains("pipeBind2"),
            "single-arg translate pipe should compile to pipeBind2: {body}"
        );
        assert!(
            body.contains("tier.features['portfolios']"),
            "@for loop variable must stay unprefixed inside pipe arg: {body}"
        );
        assert!(
            !body.contains("ctx.tier"),
            "must not emit ctx.tier inside the @for body: {body}"
        );
    }

    #[test]
    fn test_pipe_positional_arg_preserves_for_local() {
        // Same principle with a non-object positional argument.
        let comp = test_component();
        let nodes = vec![TemplateNode::ForBlock(ForBlockNode {
            item_name: "item".to_string(),
            iterable: "items()".to_string(),
            track_expression: "item".to_string(),
            children: vec![TemplateNode::Interpolation(InterpolationNode {
                expression: "item.date".to_string(),
                pipes: vec![PipeCall {
                    name: "date".to_string(),
                    args: vec!["item.format".to_string()],
                }],
            })],
            empty_children: None,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let body = output.child_template_functions.join("\n");
        assert!(
            body.contains("item.format"),
            "@for loop variable used as positional pipe arg must stay unprefixed: {body}"
        );
        assert!(
            !body.contains("ctx.item.format"),
            "must not emit ctx.item.format: {body}"
        );
    }

    #[test]
    fn test_template_reference_var_resolves_via_ivy_reference() {
        // `<form #profileForm="ngForm"><button [disabled]="!profileForm.form.valid"></button></form>`
        // The ref must:
        //   - appear in consts as ['profileForm','ngForm']
        //   - be passed as the 4th arg to elementStart on the form
        //   - resolve in the update block via ɵɵreference(slot) — NOT ctx.profileForm
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "form".to_string(),
            attributes: vec![TemplateAttribute::Reference {
                name: "profileForm".to_string(),
                export_as: Some("ngForm".to_string()),
            }],
            children: vec![TemplateNode::Element(ElementNode {
                tag: "button".to_string(),
                attributes: vec![TemplateAttribute::Property {
                    name: "disabled".to_string(),
                    expression: "!profileForm.form.valid".to_string(),
                }],
                children: vec![],
                is_void: false,
            })],
            is_void: false,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let dc = &output.static_fields[0];
        assert!(
            output
                .consts
                .iter()
                .any(|c| c.contains("'profileForm'") && c.contains("'ngForm'")),
            "consts should include ref entry ['profileForm','ngForm']: {:?}",
            output.consts
        );
        let form_start = dc
            .lines()
            .find(|l| l.contains("elementStart") && l.contains("'form'"))
            .unwrap_or("");
        assert!(
            form_start.matches(',').count() >= 3,
            "form elementStart must pass refsIdx as 4th arg: {form_start}"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}reference("),
            "update block should emit ɵɵreference(slot) for ref use: {dc}"
        );
        assert!(
            !dc.contains("ctx.profileForm"),
            "ref name must not be prefixed with ctx.: {dc}"
        );
    }

    #[test]
    fn test_valueless_attribute_included_in_consts() {
        // Value-less attributes (e.g. `baseChart` on `<canvas baseChart>`)
        // must appear in the consts array with an empty-string value so that
        // directive selectors like `canvas[baseChart]` can match at runtime.
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "canvas".to_string(),
            attributes: vec![TemplateAttribute::Static {
                name: "baseChart".to_string(),
                value: None,
            }],
            children: vec![],
            is_void: false,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let dc = &output.static_fields[0];
        // The consts array should contain the attribute pair
        assert!(
            output
                .consts
                .iter()
                .any(|c| c.contains("'baseChart'") && c.contains("''")),
            "consts should include valueless attribute: {:?}",
            output.consts
        );
        // The elementStart call should reference a consts index
        assert!(
            dc.contains("elementStart(0, 'canvas', 0)"),
            "elementStart should reference consts index for baseChart attr: {dc}"
        );
    }

    /// Regression: GitHub #69 — property-binding pipe and interpolation pipe in
    /// the same template must receive distinct `slotOffset` arguments. Before
    /// the fix, `replace_pipes_in_expr` used `var_count` as the pipe offset
    /// while `wrap_with_pipes` used `pipe_var_offset`; `rewrite_pipe_offsets`
    /// then double-counted the sequential bindings and both pipes landed on
    /// the same LView slot, triggering NG0100.
    #[test]
    fn test_article_shaped_pipes_have_distinct_slot_offsets() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "a".to_string(),
            attributes: vec![TemplateAttribute::Property {
                name: "href".to_string(),
                expression: "article.url | safeUrl".to_string(),
            }],
            children: vec![TemplateNode::Element(ElementNode {
                tag: "span".to_string(),
                attributes: Vec::new(),
                children: vec![TemplateNode::Interpolation(InterpolationNode {
                    expression: "article.datetime * 1000".to_string(),
                    pipes: vec![PipeCall {
                        name: "date".to_string(),
                        args: vec!["'short'".to_string()],
                    }],
                })],
                is_void: false,
            })],
            is_void: false,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let dc = &output.static_fields[0];

        let offsets = collect_pipe_slot_offsets(dc);
        assert_eq!(
            offsets.len(),
            2,
            "expected two pipeBind* calls in the template: {dc}"
        );
        assert_ne!(
            offsets[0], offsets[1],
            "pipeBind slot offsets must be distinct (got {offsets:?}): {dc}"
        );

        // vars: must cover seq bindings (1 property + 1 textInterpolate = 2)
        // plus pipe slots (2 for pipeBind1 + 3 for pipeBind2 = 5) = 7.
        assert!(
            dc.contains("vars: 7"),
            "vars: should equal total binding slots (2 seq + 5 pipe = 7): {dc}"
        );

        // Both pipe offsets must fall inside the binding range, strictly after
        // the sequential bindings (which occupy slots 0..=1).
        for off in &offsets {
            assert!(
                *off >= 2,
                "pipe offset {off} must sit after sequential bindings: {dc}"
            );
            assert!(
                *off < 7,
                "pipe offset {off} must fit in vars: 7 range: {dc}"
            );
        }
    }

    /// Regression: two top-level pipes in successive property bindings on the
    /// same element (both flowing through `replace_pipes_in_expr`) must also
    /// receive distinct `slotOffset` values.
    #[test]
    fn test_two_property_binding_pipes_have_distinct_slot_offsets() {
        let comp = test_component();
        let nodes = vec![TemplateNode::Element(ElementNode {
            tag: "img".to_string(),
            attributes: vec![
                TemplateAttribute::Property {
                    name: "src".to_string(),
                    expression: "photo.url | safeUrl".to_string(),
                },
                TemplateAttribute::Property {
                    name: "alt".to_string(),
                    expression: "photo.caption | translate".to_string(),
                },
            ],
            children: Vec::new(),
            is_void: true,
        })];
        let output = generate_ivy(&comp, &nodes).expect("should generate");
        let dc = &output.static_fields[0];
        let offsets = collect_pipe_slot_offsets(dc);
        assert_eq!(offsets.len(), 2, "expected two pipeBind* calls: {dc}");
        assert_ne!(
            offsets[0], offsets[1],
            "successive property pipes must have distinct offsets (got {offsets:?}): {dc}"
        );
    }

    #[test]
    fn test_animation_property_binding_codegen() {
        let output = compile_template("<div [@fade]=\"state\"></div>");
        let dc = &output.static_fields[0];
        assert!(
            dc.contains("\u{0275}\u{0275}property('@fade', ctx.state)"),
            "expected ɵɵproperty('@fade', ctx.state): {dc}"
        );
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}property"));
    }

    #[test]
    fn test_animation_listener_done_codegen() {
        let output = compile_template("<div (@fade.done)=\"onDone($event)\"></div>");
        let dc = &output.static_fields[0];
        assert!(
            dc.contains("\u{0275}\u{0275}listener('@fade.done'"),
            "expected ɵɵlistener('@fade.done', ...): {dc}"
        );
        assert!(
            dc.contains("ctx.onDone($event)"),
            "listener body should call ctx.onDone($event): {dc}"
        );
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}listener"));
    }

    #[test]
    fn test_animation_listener_start_codegen() {
        let output = compile_template("<div (@fade.start)=\"onStart()\"></div>");
        let dc = &output.static_fields[0];
        assert!(
            dc.contains("\u{0275}\u{0275}listener('@fade.start'"),
            "expected ɵɵlistener('@fade.start', ...): {dc}"
        );
    }

    #[test]
    fn test_defer_on_viewport_emits_trigger_instruction() {
        let output = compile_template("@defer (on viewport) { <my-c /> }");
        let dc = &output.static_fields[0];
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}defer"),
            "defer should be imported"
        );
        assert!(
            output
                .ivy_imports
                .contains("\u{0275}\u{0275}deferOnViewport"),
            "deferOnViewport should be imported"
        );
        assert!(dc.contains("\u{0275}\u{0275}defer(0, "), "defer call: {dc}");
        assert!(
            dc.contains("\u{0275}\u{0275}deferOnViewport();"),
            "deferOnViewport call: {dc}"
        );
    }

    #[test]
    fn test_defer_on_idle_emits_trigger_instruction() {
        let output = compile_template("@defer (on idle) { <my-c /> }");
        assert!(output.ivy_imports.contains("\u{0275}\u{0275}deferOnIdle"));
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}deferOnIdle();"));
    }

    #[test]
    fn test_defer_on_timer_emits_ms_argument() {
        let output = compile_template("@defer (on timer(500ms)) { <my-c /> }");
        let dc = &output.static_fields[0];
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}deferOnTimer"),
            "deferOnTimer should be imported"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}deferOnTimer(500);"),
            "timer arg should parse to 500ms: {dc}"
        );
    }

    #[test]
    fn test_defer_on_timer_seconds_converts_to_ms() {
        let output = compile_template("@defer (on timer(2s)) { <my-c /> }");
        assert!(output.static_fields[0].contains("\u{0275}\u{0275}deferOnTimer(2000);"));
    }

    #[test]
    fn test_defer_when_emits_in_update_block() {
        let output = compile_template("@defer (when isReady) { <my-c /> }");
        let dc = &output.static_fields[0];
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}deferWhen"),
            "deferWhen should be imported"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}deferWhen(ctx.isReady);"),
            "deferWhen should wrap condition with ctx.: {dc}"
        );
        // `when` triggers contribute to update-block binding count.
        assert!(dc.contains("vars: 1"), "update binding counted: {dc}");
    }

    #[test]
    fn test_defer_prefetch_on_idle_uses_prefetch_instruction() {
        let output = compile_template("@defer (prefetch on idle; on viewport) { <my-c /> }");
        let dc = &output.static_fields[0];
        assert!(output
            .ivy_imports
            .contains("\u{0275}\u{0275}deferPrefetchOnIdle"));
        assert!(dc.contains("\u{0275}\u{0275}deferPrefetchOnIdle();"));
        assert!(dc.contains("\u{0275}\u{0275}deferOnViewport();"));
    }

    #[test]
    fn test_defer_all_sub_blocks_emit_separate_templates() {
        let output = compile_template(
            "@defer (on idle) { <my-c /> } @placeholder { <p>wait</p> } @loading { <p>loading</p> } @error { <p>err</p> }",
        );
        let dc = &output.static_fields[0];
        // One template call per sub-block plus main = 4 ɵɵtemplate calls.
        assert_eq!(
            dc.matches("\u{0275}\u{0275}template(").count(),
            4,
            "expected 4 template calls (main + 3 sub-blocks): {dc}"
        );
        // Child template functions include the four sub-templates plus the
        // dep fn helper.
        let child_source = output.child_template_functions.join("\n");
        assert!(child_source.contains("TestComponent_Defer_"));
        assert!(child_source.contains("TestComponent_DeferPlaceholder_"));
        assert!(child_source.contains("TestComponent_DeferLoading_"));
        assert!(child_source.contains("TestComponent_DeferError_"));
        assert!(child_source.contains("DepsFn"));
        // defer passes all three sub-block slot indices as trailing args.
        assert!(
            dc.contains("\u{0275}\u{0275}defer(0, "),
            "defer block at slot 0: {dc}"
        );
    }

    #[test]
    fn test_defer_dep_fn_emits_import_for_custom_tag() {
        let output = compile_template("@defer (on idle) { <my-c /> }");
        let deps_fn = output
            .child_template_functions
            .iter()
            .find(|f| f.contains("DepsFn"))
            .expect("dep fn should be emitted");
        assert!(
            deps_fn.contains("import('./my-c')"),
            "dep fn should contain dynamic import for <my-c>: {deps_fn}"
        );
        assert!(
            deps_fn.contains("m => m.MyC"),
            "dep fn should reference PascalCase symbol: {deps_fn}"
        );
    }

    #[test]
    fn test_defer_dep_fn_empty_without_custom_tags() {
        let output = compile_template("@defer (on idle) { <p>hello</p> }");
        let deps_fn = output
            .child_template_functions
            .iter()
            .find(|f| f.contains("DepsFn"))
            .expect("dep fn should be emitted");
        assert!(
            deps_fn.contains("return [];"),
            "dep fn with no custom tags should return []: {deps_fn}"
        );
    }

    fn collect_pipe_slot_offsets(code: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for marker in [
            "\u{0275}\u{0275}pipeBind1(",
            "\u{0275}\u{0275}pipeBind2(",
            "\u{0275}\u{0275}pipeBind3(",
            "\u{0275}\u{0275}pipeBindV(",
        ] {
            let mut cursor = 0;
            while let Some(rel) = code[cursor..].find(marker) {
                let after = cursor + rel + marker.len();
                let rest = &code[after..];
                let comma = rest.find(',').expect("pipeBind* should have ≥ 2 args");
                let offset_region = rest[comma + 1..].trim_start();
                let digits: String = offset_region
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                out.push(
                    digits
                        .parse::<u32>()
                        .expect("second arg to pipeBind* must be a literal offset"),
                );
                cursor = after;
            }
        }
        out
    }

    #[test]
    fn test_i18n_element_emits_localize_and_i18n_instruction() {
        let output = compile_template("<h1 i18n>Hello</h1>");
        let dc = &output.static_fields[0];
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}i18n"),
            "ɵɵi18n should be imported"
        );
        assert!(
            dc.contains("$localize`Hello`"),
            "consts should contain $localize`Hello`: {dc}"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}i18n(1, 0);"),
            "ɵɵi18n(1, 0) should reference consts[0]: {dc}"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}elementStart(0, 'h1');"),
            "elementStart for h1: {dc}"
        );
    }

    #[test]
    fn test_i18n_element_with_meta_emits_prefix() {
        let output = compile_template(r#"<h1 i18n="greeting|welcome@@intro">Hi</h1>"#);
        let dc = &output.static_fields[0];
        assert!(
            dc.contains("$localize`:greeting|welcome@@intro:Hi`"),
            "meta prefix should be emitted in the localize literal: {dc}"
        );
    }

    #[test]
    fn test_i18n_element_interpolation_emits_i18n_exp_apply() {
        let output = compile_template("<p i18n>Hello, {{ name }}!</p>");
        let dc = &output.static_fields[0];
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}i18nExp"),
            "ɵɵi18nExp should be imported"
        );
        assert!(
            output.ivy_imports.contains("\u{0275}\u{0275}i18nApply"),
            "ɵɵi18nApply should be imported"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}i18nExp(ctx.name);"),
            "update block should emit ɵɵi18nExp(ctx.name): {dc}"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}i18nApply(1);"),
            "update block should emit ɵɵi18nApply(slot): {dc}"
        );
        assert!(
            dc.contains(":INTERPOLATION:"),
            "placeholder marker should be in the message: {dc}"
        );
    }

    #[test]
    fn test_i18n_attr_emits_i18n_attributes_instruction() {
        let output = compile_template(r#"<img alt="Hello" i18n-alt="@@alt-id" />"#);
        let dc = &output.static_fields[0];
        assert!(
            output
                .ivy_imports
                .contains("\u{0275}\u{0275}i18nAttributes"),
            "ɵɵi18nAttributes should be imported"
        );
        assert!(
            dc.contains("\u{0275}\u{0275}i18nAttributes(0,"),
            "ɵɵi18nAttributes should target element slot 0: {dc}"
        );
        assert!(
            dc.contains("$localize`:@@alt-id:Hello`"),
            "consts should carry the attribute's localize message: {dc}"
        );
        assert!(
            dc.contains("[6, 'alt', "),
            "AttributeMarker.I18n (6) entry should list the attr name: {dc}"
        );
    }

    #[test]
    fn test_icu_plural_inside_i18n_is_inlined() {
        let output = compile_template(
            "<span i18n>{ count, plural, =0 {none} =1 {one} other {many} }</span>",
        );
        let dc = &output.static_fields[0];
        assert!(
            dc.contains("{count, plural, "),
            "ICU body should be inlined into the localize message: {dc}"
        );
        assert!(
            dc.contains("=0 {none}") && dc.contains("other {many}"),
            "ICU case bodies should be preserved: {dc}"
        );
    }

    #[test]
    fn test_scope_component_styles_single_element_array() {
        let scoped = scope_component_styles("[`.a { color: red; }`]");
        assert_eq!(scoped, "[`.a[_ngcontent-%COMP%]{ color: red; }`]");
    }

    #[test]
    fn test_scope_component_styles_multi_element_array_preserves_boundaries() {
        // Regression guard for GH #81. With two template literals in the
        // array, the old `find('`')..rfind('`')` logic treated the whole
        // middle as CSS and emitted `[_ngcontent-%COMP%]` OUTSIDE a template
        // literal, producing invalid TypeScript. Each literal must be
        // scoped independently.
        let scoped = scope_component_styles("[`.a { color: red; }`, `.b { color: blue; }`]");
        assert_eq!(
            scoped,
            "[`.a[_ngcontent-%COMP%]{ color: red; }`, `.b[_ngcontent-%COMP%]{ color: blue; }`]"
        );
    }

    #[test]
    fn test_scope_component_styles_skips_interpolation() {
        // Template literal with `${expr}` spans JS, not CSS; the scoper
        // must leave it alone.
        let src = r#"[`.a { width: ${n}px; }`]"#;
        let scoped = scope_component_styles(src);
        assert_eq!(scoped, src);
    }

    #[test]
    fn test_scope_component_styles_handles_string_literals() {
        let scoped = scope_component_styles("['.a { color: red; }']");
        assert_eq!(scoped, "['.a[_ngcontent-%COMP%]{ color: red; }']");
    }

    #[test]
    fn test_scope_component_styles_host_selector() {
        let scoped = scope_component_styles("[`:host { display: block; }`]");
        assert_eq!(scoped, "[`[_nghost-%COMP%]{ display: block; }`]");
    }
}
