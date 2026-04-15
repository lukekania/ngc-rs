use std::collections::BTreeSet;

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
    /// Nesting depth of embedded views (for `ɵɵnextContext(depth)` calls).
    /// 0 = root template, 1 = inside one @if/@for, 2 = nested @if inside @for, etc.
    embed_depth: u32,
    /// Stack of @for ancestor item variables with their embed depth.
    /// Used to generate intermediate ɵɵnextContext() calls in nested templates
    /// to access @for loop variables (e.g. `h` from `@for (h of holdings)`).
    for_var_stack: Vec<(String, u32)>,
    /// Last slot index emitted in the update block (for computing `ɵɵadvance` deltas).
    /// `None` means no advance has been emitted yet (runtime starts at selectedIndex=-1).
    last_update_slot: Option<u32>,
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
        embed_depth: 0,
        for_var_stack: Vec::new(),
        last_update_slot: None,
    };

    gen.ivy_imports
        .insert("\u{0275}\u{0275}defineComponent".to_string());

    gen.generate_nodes(template_nodes);

    let decls = gen.slot_index;
    let vars = gen.var_count;

    // Build template function body
    let mut template_body = String::new();
    if !gen.creation.is_empty() {
        template_body.push_str("    if (rf & 1) {\n");
        for instr in &gen.creation {
            template_body.push_str("      ");
            template_body.push_str(instr);
            template_body.push('\n');
        }
        template_body.push_str("    }\n");
    }
    if !gen.update.is_empty() {
        template_body.push_str("    if (rf & 2) {\n");
        for instr in &gen.update {
            template_body.push_str("      ");
            template_body.push_str(instr);
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
    // Generate inputs property from @Input() decorated properties
    if !component.input_properties.is_empty() {
        let inputs: Vec<String> = component
            .input_properties
            .iter()
            .map(|p| format!("{p}: '{p}'"))
            .collect();
        dc.push_str(&format!("    inputs: {{ {} }},\n", inputs.join(", ")));
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

    // For standalone components, use getComponentDepsFactory to resolve NgModule imports
    // to their exported directives/pipes at runtime via the depsTracker.
    // rawImports must be the direct array, not wrapped in a function.
    if let Some(ref imports_src) = component.imports_source {
        if component.standalone {
            gen.ivy_imports
                .insert("\u{0275}\u{0275}getComponentDepsFactory".to_string());
            dc.push_str(&format!(
                ",\n    dependencies: \u{0275}\u{0275}getComponentDepsFactory({}, {imports_src})",
                component.class_name
            ));
        } else {
            dc.push_str(&format!(",\n    dependencies: () => {imports_src}"));
        }
    }
    if let Some(ref styles_src) = component.styles_source {
        dc.push_str(&format!(",\n    styles: {styles_src}"));
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
        for node in nodes {
            self.generate_node(node);
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
        }
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

        // Special Angular elements
        match el.tag.as_str() {
            "ng-content" => {
                let slot = self.slot_index;
                self.slot_index += 1;
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
                self.creation.push(format!(
                    "\u{0275}\u{0275}template({slot}, {fn_name}, {}, {});",
                    child.decls, child.vars
                ));
                self.child_templates.push(child);
                return;
            }
            _ => {}
        }

        let slot = self.slot_index;
        self.slot_index += 1;

        let is_ng_container = el.tag == "ng-container";

        // Check for event bindings (used for listener creation)
        let _has_events = el
            .attributes
            .iter()
            .any(|a| matches!(a, TemplateAttribute::Event { .. }));

        // Static attributes for consts
        let static_attrs: Vec<(&str, &str)> = el
            .attributes
            .iter()
            .filter_map(|a| match a {
                TemplateAttribute::Static {
                    name,
                    value: Some(v),
                } => Some((name.as_str(), v.as_str())),
                _ => None,
            })
            .collect();

        if el.is_void && !is_ng_container {
            let instr = if is_ng_container {
                "\u{0275}\u{0275}elementContainer"
            } else {
                "\u{0275}\u{0275}element"
            };
            self.ivy_imports.insert(instr.to_string());
            if static_attrs.is_empty() {
                self.creation
                    .push(format!("{instr}({slot}, '{}');", el.tag));
            } else {
                let const_idx = self.register_const(&static_attrs);
                self.creation
                    .push(format!("{instr}({slot}, '{}', {const_idx});", el.tag));
            }

            // Bindings for void elements
            self.emit_element_bindings(el, slot);
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
                // ng-container has no DOM tag — only pass slot and optional consts index
                if static_attrs.is_empty() {
                    self.creation.push(format!("{start_instr}({slot});"));
                } else {
                    let const_idx = self.register_const(&static_attrs);
                    self.creation
                        .push(format!("{start_instr}({slot}, {const_idx});"));
                }
            } else if static_attrs.is_empty() {
                self.creation
                    .push(format!("{start_instr}({slot}, '{}');", el.tag));
            } else {
                let const_idx = self.register_const(&static_attrs);
                self.creation
                    .push(format!("{start_instr}({slot}, '{}', {const_idx});", el.tag));
            }

            // Event listeners and two-way binding listeners in creation block
            for attr in &el.attributes {
                match attr {
                    TemplateAttribute::Event { name, handler } => {
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}listener".to_string());
                        let compiled_handler = compile_event_handler(handler);
                        if self.embed_depth > 0 {
                            // Inside embedded view: restore view and get component ctx
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}restoreView".to_string());
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}nextContext".to_string());
                            let depth = self.embed_depth;
                            self.creation.push(format!(
                                "\u{0275}\u{0275}listener('{}', function($event) {{ \u{0275}\u{0275}restoreView(_r); const ctx = \u{0275}\u{0275}nextContext({depth}); {compiled_handler} }});",
                                name,
                            ));
                        } else {
                            // Root template: ctx is the function parameter
                            self.creation.push(format!(
                                "\u{0275}\u{0275}listener('{}', function($event) {{ {compiled_handler} }});",
                                name,
                            ));
                        }
                    }
                    TemplateAttribute::TwoWayBinding { name, expression } => {
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}listener".to_string());
                        if self.embed_depth > 0 {
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}restoreView".to_string());
                            self.ivy_imports
                                .insert("\u{0275}\u{0275}nextContext".to_string());
                            let depth = self.embed_depth;
                            self.creation.push(format!(
                                "\u{0275}\u{0275}listener('{}Change', function($event) {{ \u{0275}\u{0275}restoreView(_r); const ctx = \u{0275}\u{0275}nextContext({depth}); return {} = $event; }});",
                                name, ctx_expr(expression)
                            ));
                        } else {
                            self.creation.push(format!(
                                "\u{0275}\u{0275}listener('{}Change', function($event) {{ return {} = $event; }});",
                                name, ctx_expr(expression)
                            ));
                        }
                    }
                    _ => {}
                }
            }

            // Property bindings in update block — emitted before children
            // so ɵɵadvance() targets the correct element slot.
            self.emit_element_bindings(el, slot);

            // Generate children
            self.generate_nodes(&el.children);

            self.creation.push(format!("{end_instr}();"));

            // Template reference variables
            for attr in &el.attributes {
                if let TemplateAttribute::Reference { .. } = attr {
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}reference".to_string());
                    let ref_slot = self.slot_index;
                    self.slot_index += 1;
                    self.creation
                        .push(format!("\u{0275}\u{0275}reference({ref_slot});"));
                }
            }
        }
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
        let parent_last_update: Option<u32> = self.last_update_slot;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);
        let parent_consts = std::mem::take(&mut self.consts);
        let parent_lets = self.let_declarations.clone();

        self.slot_index = 0;
        self.var_count = 0;
        self.last_update_slot = None;

        self.generate_element(el);

        let decls = self.slot_index;
        let vars = self.var_count + parent_lets.len() as u32;

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

        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.last_update_slot = parent_last_update;
        self.creation = parent_creation;
        self.update = parent_update;
        self.consts = parent_consts;
        self.let_declarations = parent_lets;

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

        let escaped = escape_js_string(&text.value);
        self.creation
            .push(format!("\u{0275}\u{0275}text({slot}, '{escaped}');"));
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
            .insert("\u{0275}\u{0275}conditionalCreate".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}conditional".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        // Generate child template for the @if body
        let child_fn_name = format!(
            "{}_Conditional_{}_Template",
            self.component_name, self.child_counter
        );
        self.child_counter += 1;

        let child = self.generate_child_template(&child_fn_name, &block.children);
        self.creation.push(format!(
            "\u{0275}\u{0275}conditionalCreate({slot}, {child_fn_name}, {}, {});",
            child.decls, child.vars
        ));
        self.child_templates.push(child);

        // Generate else-if and else child templates
        let mut else_if_slots = Vec::new();
        for (_i, branch) in block.else_if_branches.iter().enumerate() {
            let fn_name = format!(
                "{}_ConditionalElseIf_{}_Template",
                self.component_name, self.child_counter
            );
            self.child_counter += 1;
            self.ivy_imports
                .insert("\u{0275}\u{0275}conditionalBranchCreate".to_string());
            let ei_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, &branch.children);
            self.creation.push(format!(
                "\u{0275}\u{0275}conditionalBranchCreate({ei_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
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
                .insert("\u{0275}\u{0275}conditionalBranchCreate".to_string());
            let else_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, else_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}conditionalBranchCreate({else_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
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

        // @empty block — passed as extra args to ɵɵrepeaterCreate
        if let Some(ref empty_children) = block.empty_children {
            let empty_fn_name = format!("{}_ForEmpty_{}_Template", self.component_name, slot);
            let empty_child = self.generate_child_template(&empty_fn_name, empty_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}repeaterCreate({slot}, {child_fn_name}, {}, {}, null, null, {track_fn}, false, {empty_fn_name}, {}, {});",
                child.decls, child.vars, empty_child.decls, empty_child.vars
            ));
            self.child_templates.push(empty_child);
        } else {
            self.creation.push(format!(
                "\u{0275}\u{0275}repeaterCreate({slot}, {child_fn_name}, {}, {}, null, null, {track_fn});",
                child.decls, child.vars
            ));
        }
        self.child_templates.push(child);

        self.add_advance(slot);
        self.update.push(format!(
            "\u{0275}\u{0275}repeater({});",
            ctx_expr(&block.iterable)
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
                    .insert("\u{0275}\u{0275}conditionalBranchCreate".to_string());
                self.creation.push(format!(
                    "\u{0275}\u{0275}conditionalBranchCreate({case_slot}, {fn_name}, {}, {});",
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
                .insert("\u{0275}\u{0275}conditionalBranchCreate".to_string());
            let default_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, default_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}conditionalBranchCreate({default_slot}, {fn_name}, {}, {});",
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
                ctx_expr(&block.expression),
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
        let parent_last_update: Option<u32> = self.last_update_slot;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);
        let parent_lets = self.let_declarations.clone();
        self.embed_depth += 1;

        self.slot_index = 0;
        self.var_count = 0;
        self.last_update_slot = None;
        // Don't clear let_declarations, local_vars, or consts — children
        // inherit parent scope and share the component-level consts array.

        self.generate_nodes(children);

        let decls = self.slot_index;
        let vars = self.var_count + parent_lets.len() as u32;

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
            // Navigate up the view tree, extracting @for loop variables along the way.
            // Each ɵɵnextContext() call is stateful (moves pointer up), so we go
            // step by step through @for ancestors before reaching the component.
            let current_depth = self.embed_depth;
            let mut remaining = current_depth;
            // Sort for_var_stack by depth descending (closest ancestor first)
            let mut for_vars: Vec<(String, u32)> = self.for_var_stack.clone();
            for_vars.sort_by(|a, b| b.1.cmp(&a.1));
            for (var_name, var_depth) in &for_vars {
                let steps = remaining - var_depth;
                if steps > 0 {
                    code.push_str(&format!(
                        "    const _{var_name}_ctx = \u{0275}\u{0275}nextContext({steps});\n"
                    ));
                    code.push_str(&format!(
                        "    const {var_name} = _{var_name}_ctx.$implicit;\n"
                    ));
                    remaining = *var_depth;
                }
            }
            // Navigate remaining levels to the component
            if remaining > 0 {
                code.push_str(&format!(
                    "    const ctx = \u{0275}\u{0275}nextContext({remaining});\n"
                ));
            } else {
                code.push_str("    const ctx = \u{0275}\u{0275}nextContext();\n");
            }
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

        // Restore parent state (consts is NOT restored — shared)
        self.embed_depth -= 1;
        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.last_update_slot = parent_last_update;
        self.creation = parent_creation;
        self.update = parent_update;
        self.let_declarations = parent_lets;

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
        let parent_last_update: Option<u32> = self.last_update_slot;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);
        let parent_lets = self.let_declarations.clone();
        self.embed_depth += 1;

        self.slot_index = 0;
        self.var_count = 0;
        self.last_update_slot = None;

        // Register the @for item variable as a local so ctx_expr_with_locals()
        // does NOT prefix it with `ctx.`.  e.g. `p.id` stays `p.id`, not `ctx.p.id`.
        let parent_locals = self.local_vars.clone();
        self.local_vars.insert(item_name.to_string());
        // Track this @for's item variable and its depth for nested templates
        self.for_var_stack
            .push((item_name.to_string(), self.embed_depth));

        self.generate_nodes(children);

        let decls = self.slot_index;
        let vars = self.var_count + parent_lets.len() as u32;

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
            // @for child templates receive RepeaterContext as _ctx ($implicit, $index).
            // Rebind `ctx` to the parent component via ɵɵnextContext(depth) so that
            // ctx_expr()-generated references like `ctx.someMethod()` work.
            let depth = self.embed_depth;
            self.ivy_imports
                .insert("\u{0275}\u{0275}nextContext".to_string());
            code.push_str(&format!(
                "    const ctx = \u{0275}\u{0275}nextContext({depth});\n"
            ));
            code.push_str(&format!("    const {item_name} = _ctx.$implicit;\n"));
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

        // Restore parent state (consts is NOT restored — shared)
        self.embed_depth -= 1;
        self.for_var_stack.pop();
        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.last_update_slot = parent_last_update;
        self.creation = parent_creation;
        self.update = parent_update;
        self.let_declarations = parent_lets;
        self.local_vars = parent_locals;

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
            // Capture binding start BEFORE incrementing
            let pipe_var_slot = self.var_count;
            // Each pipe uses 2 + args binding slots: input + pure cache + extra args
            self.var_count += 2 + pipe.args.len() as u32;

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
                    .map(|a| {
                        let trimmed = a.trim();
                        if trimmed.starts_with('{') {
                            let wrapped = format!("({})", trimmed);
                            let result = ctx_expr(&wrapped);
                            result
                                .strip_prefix('(')
                                .and_then(|s| s.strip_suffix(')'))
                                .unwrap_or(&result)
                                .to_string()
                        } else {
                            ctx_expr(a)
                        }
                    })
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
            // No pipes found — just compile with ctx. prefix
            return ctx_expr_with_locals(expression, &self.local_vars);
        }

        // First segment is the base expression, rest are pipe names
        // But pipes can be nested in sub-expressions. We need to replace
        // pipe segments bottom-up. For simplicity, handle the common pattern:
        // the entire expression or sub-expressions of form `(expr | pipe)`.
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
            // Capture binding start BEFORE incrementing
            let pipe_var_slot = self.var_count;
            // Each pipe uses 2 + args binding slots: input + pure cache + extra args
            self.var_count += 2 + args.len() as u32;
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
                .map(|a| {
                    let trimmed = a.trim();
                    if trimmed.starts_with('{') {
                        // Wrap object literals in parens so oxc parses them as
                        // expressions, not block statements.
                        let wrapped = format!("({})", trimmed);
                        let result = ctx_expr(&wrapped);
                        result
                            .strip_prefix('(')
                            .and_then(|s| s.strip_suffix(')'))
                            .unwrap_or(&result)
                            .to_string()
                    } else {
                        ctx_expr(a)
                    }
                })
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

    /// Add an `ɵɵadvance()` instruction to the update block.
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
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}property".to_string());
                    let compiled = self.compile_binding_expr(expression);
                    self.update
                        .push(format!("\u{0275}\u{0275}property('{}', {compiled});", name,));
                    self.var_count += 1;
                }
                TemplateAttribute::TwoWayBinding { name, expression } => {
                    self.ivy_imports
                        .insert("\u{0275}\u{0275}property".to_string());
                    let compiled = self.compile_binding_expr(expression);
                    self.update
                        .push(format!("\u{0275}\u{0275}property('{}', {compiled});", name,));
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
                let compiled_base = ctx_expr(&compiled_base);

                let pipe_slot = gen.slot_index;
                gen.slot_index += 1;
                let pipe_var_slot = gen.var_count;
                gen.var_count += 2 + args.len() as u32;
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
                .map(|a| {
                    let trimmed = a.trim();
                    if trimmed.starts_with('{') {
                        // Wrap object literals in parens so oxc parses them as
                        // expressions, not block statements.
                        let wrapped = format!("({})", trimmed);
                        let result = ctx_expr(&wrapped);
                        result
                            .strip_prefix('(')
                            .and_then(|s| s.strip_suffix(')'))
                            .unwrap_or(&result)
                            .to_string()
                    } else {
                        ctx_expr(a)
                    }
                })
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
        Expression::Identifier(id) => {
            if !is_member_property && !is_builtin(&id.name) && !is_local(&id.name) {
                ctx_inserts.push(id.span.start);
            }
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
                    collect_ctx_rewrites(
                        &call.callee,
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
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
                    collect_ctx_rewrites(
                        &member.object,
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
                }
                ChainElement::ComputedMemberExpression(member) => {
                    collect_ctx_rewrites(
                        &member.object,
                        ctx_inserts,
                        remove_ranges,
                        false,
                        locals,
                    );
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
    let mut expr = format!("{} ? {} : ", ctx_expr_with_locals(condition, locals), if_slot);

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
fn format_static_attrs(attrs: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = attrs
        .iter()
        .flat_map(|(k, v)| {
            vec![
                format!("'{}'", escape_js_string(k)),
                format!("'{}'", escape_js_string(v)),
            ]
        })
        .collect();
    format!("[{}]", pairs.join(", "))
}

/// Compile an Angular event handler expression.
///
/// Handles multi-statement handlers like `$event.stopPropagation(); doSomething()`
/// by splitting on `;`, applying `ctx.` to each statement, and adding `return` to
/// the last statement.
fn compile_event_handler(handler: &str) -> String {
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
        let compiled = ctx_expr(stmt);
        if i == statements.len() - 1 {
            parts.push(format!("return {compiled};"));
        } else {
            parts.push(format!("{compiled};"));
        }
    }

    parts.join(" ")
}

/// Escape a string for use inside a single-quoted JavaScript string literal.
fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
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
        }
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
}
