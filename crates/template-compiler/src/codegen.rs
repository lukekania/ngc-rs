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
    dc.push_str(&format!("    decls: {decls},\n"));
    dc.push_str(&format!("    vars: {vars},\n"));
    dc.push_str(&format!(
        "    template: function {}_Template(rf: number, ctx: {}) {{\n",
        component.class_name, component.class_name
    ));
    dc.push_str(&template_body);
    dc.push_str("    }");

    // Add dependencies if imports exist
    if let Some(ref imports_src) = component.imports_source {
        dc.push_str(&format!(",\n    dependencies: {imports_src}"));
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

        // Collect event and update bindings
        let _has_events = el
            .attributes
            .iter()
            .any(|a| matches!(a, TemplateAttribute::Event { .. }));
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
                let attrs_str = format_static_attrs(&static_attrs);
                self.creation
                    .push(format!("{instr}({slot}, '{}', {attrs_str});", el.tag));
            }
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
            if static_attrs.is_empty() {
                self.creation
                    .push(format!("{start_instr}({slot}, '{}');", el.tag));
            } else {
                let attrs_str = format_static_attrs(&static_attrs);
                self.creation
                    .push(format!("{start_instr}({slot}, '{}', {attrs_str});", el.tag));
            }

            // Event listeners and two-way binding listeners in creation block
            for attr in &el.attributes {
                match attr {
                    TemplateAttribute::Event { name, handler } => {
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}listener".to_string());
                        self.creation.push(format!(
                            "\u{0275}\u{0275}listener('{}', function() {{ return ctx.{}; }});",
                            name, handler
                        ));
                    }
                    TemplateAttribute::TwoWayBinding { name, expression } => {
                        self.ivy_imports
                            .insert("\u{0275}\u{0275}listener".to_string());
                        self.creation.push(format!(
                            "\u{0275}\u{0275}listener('{}Change', function($event) {{ return {} = $event; }});",
                            name, ctx_expr(expression)
                        ));
                    }
                    _ => {}
                }
            }

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

        // Property bindings in update block
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
                    self.var_count += 1;
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
                    self.var_count += 1;
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
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);

        self.slot_index = 0;
        self.var_count = 0;

        self.generate_element(el);

        let decls = self.slot_index;
        let vars = self.var_count;

        let mut code = format!("function {fn_name}(rf: number, ctx: any) {{\n");
        if !self.creation.is_empty() {
            code.push_str("  if (rf & 1) {\n");
            for instr in &self.creation {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        if !self.update.is_empty() {
            code.push_str("  if (rf & 2) {\n");
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
        self.creation = parent_creation;
        self.update = parent_update;

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

        let escaped = text.value.replace('\'', "\\'");
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

        // Build the expression with pipe wrapping
        let expr = if interp.pipes.is_empty() {
            ctx_expr(&interp.expression)
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
            .insert("\u{0275}\u{0275}template".to_string());
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
            "\u{0275}\u{0275}template({slot}, {child_fn_name}, {}, {});",
            child.decls, child.vars
        ));
        self.child_templates.push(child);

        // Generate else-if and else child templates
        let mut else_if_fns = Vec::new();
        for (i, branch) in block.else_if_branches.iter().enumerate() {
            let fn_name = format!(
                "{}_ConditionalElseIf_{}_{}_Template",
                self.component_name, slot, i
            );
            let ei_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, &branch.children);
            self.creation.push(format!(
                "\u{0275}\u{0275}template({ei_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
            else_if_fns.push((branch.condition.clone(), fn_name.clone()));
            self.child_templates.push(child);
        }

        let mut else_fn_name = None;
        if let Some(ref else_children) = block.else_branch {
            let fn_name = format!("{}_ConditionalElse_{}_Template", self.component_name, slot);
            let else_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, else_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}template({else_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
            else_fn_name = Some(fn_name.clone());
            self.child_templates.push(child);
        }

        // Update block: conditional
        self.add_advance(slot);
        let cond_expr = build_conditional_expr(&block.condition, &else_if_fns, &else_fn_name);
        self.update
            .push(format!("\u{0275}\u{0275}conditional({cond_expr});"));
        self.var_count += 1;
    }

    fn generate_for_block(&mut self, block: &ForBlockNode) {
        let slot = self.slot_index;
        self.slot_index += 1;

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
        self.creation.push(format!(
            "\u{0275}\u{0275}repeaterCreate({slot}, {child_fn_name}, {}, {});",
            child.decls, child.vars
        ));
        self.child_templates.push(child);

        // @empty block
        if let Some(ref empty_children) = block.empty_children {
            let empty_fn_name = format!("{}_ForEmpty_{}_Template", self.component_name, slot);
            let empty_slot = self.slot_index;
            self.slot_index += 1;
            let empty_child = self.generate_child_template(&empty_fn_name, empty_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}template({empty_slot}, {empty_fn_name}, {}, {});",
                empty_child.decls, empty_child.vars
            ));
            self.child_templates.push(empty_child);
        }

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
            .insert("\u{0275}\u{0275}template".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}conditional".to_string());
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());

        let mut case_fns = Vec::new();
        for (i, case) in block.cases.iter().enumerate() {
            let fn_name = format!("{}_SwitchCase_{}_{}_Template", self.component_name, slot, i);
            let case_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, &case.children);
            self.creation.push(format!(
                "\u{0275}\u{0275}template({case_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
            case_fns.push((case.expression.clone(), i));
            self.child_templates.push(child);
        }

        let mut default_fn = None;
        if let Some(ref default_children) = block.default_branch {
            let fn_name = format!("{}_SwitchDefault_{}_Template", self.component_name, slot);
            let default_slot = self.slot_index;
            self.slot_index += 1;
            let child = self.generate_child_template(&fn_name, default_children);
            self.creation.push(format!(
                "\u{0275}\u{0275}template({default_slot}, {fn_name}, {}, {});",
                child.decls, child.vars
            ));
            default_fn = Some(case_fns.len());
            self.child_templates.push(child);
        }

        self.add_advance(slot);
        // Build switch conditional expression
        let mut cond = String::new();
        for (i, (expr, idx)) in case_fns.iter().enumerate() {
            if i > 0 {
                cond.push_str(" : ");
            }
            cond.push_str(&format!(
                "{} === {} ? {}",
                ctx_expr(&block.expression),
                expr,
                idx
            ));
        }
        if let Some(default_idx) = default_fn {
            cond.push_str(&format!(" : {default_idx}"));
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
        // Save parent state
        let parent_slot = self.slot_index;
        let parent_var = self.var_count;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);

        self.slot_index = 0;
        self.var_count = 0;

        self.generate_nodes(children);

        let decls = self.slot_index;
        let vars = self.var_count;

        let mut code = format!("function {fn_name}(rf: number, ctx: any) {{\n");
        if !self.creation.is_empty() {
            code.push_str("  if (rf & 1) {\n");
            for instr in &self.creation {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        if !self.update.is_empty() {
            code.push_str("  if (rf & 2) {\n");
            for instr in &self.update {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        code.push('}');

        // Restore parent state
        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.creation = parent_creation;
        self.update = parent_update;

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
        // Save parent state
        let parent_slot = self.slot_index;
        let parent_var = self.var_count;
        let parent_creation = std::mem::take(&mut self.creation);
        let parent_update = std::mem::take(&mut self.update);

        self.slot_index = 0;
        self.var_count = 0;

        self.generate_nodes(children);

        let decls = self.slot_index;
        let vars = self.var_count;

        let mut code = format!("function {fn_name}(rf: number, ctx: any) {{\n");
        if !self.creation.is_empty() {
            code.push_str("  if (rf & 1) {\n");
            for instr in &self.creation {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        if !self.update.is_empty() {
            code.push_str("  if (rf & 2) {\n");
            code.push_str(&format!("    const {item_name} = ctx.$implicit;\n"));
            for instr in &self.update {
                code.push_str("    ");
                code.push_str(instr);
                code.push('\n');
            }
            code.push_str("  }\n");
        }
        code.push('}');

        // Restore parent state
        self.slot_index = parent_slot;
        self.var_count = parent_var;
        self.creation = parent_creation;
        self.update = parent_update;

        ChildTemplate {
            function_name: fn_name.to_string(),
            decls,
            vars,
            code,
        }
    }

    fn wrap_with_pipes(&mut self, base_expr: &str, pipes: &[PipeCall]) -> String {
        let mut expr = ctx_expr(base_expr);
        for pipe in pipes {
            let pipe_slot = self.slot_index;
            self.slot_index += 1;
            self.var_count += 1 + pipe.args.len() as u32;

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

            let pipe_var_slot = self.var_count;
            if pipe.args.is_empty() {
                expr = format!("{bind_fn}({pipe_slot}, {pipe_var_slot}, {expr})");
            } else {
                let args_str = pipe.args.join(", ");
                expr = format!("{bind_fn}({pipe_slot}, {pipe_var_slot}, {expr}, {args_str})");
            }
        }
        expr
    }

    /// Compile a binding expression, handling embedded Angular pipes at any depth.
    ///
    /// Scans for `expr | pipeName` patterns (Angular pipe syntax) anywhere in the
    /// expression, compiles each to a `ɵɵpipeBind*` call, and applies `ctx.` prefixes.
    fn compile_binding_expr(&mut self, expression: &str) -> String {
        let segments = extract_all_pipe_segments(expression);
        if segments.len() <= 1 {
            // No pipes found — just compile with ctx. prefix
            return ctx_expr(expression);
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

        // Check for top-level pipe: `baseExpr | pipeName`
        if let Some((base, pipe_name)) = split_top_level_pipe(trimmed) {
            let compiled_base = self.replace_pipes_in_expr(&base);
            let pipe_slot = self.slot_index;
            self.slot_index += 1;
            self.var_count += 1;
            self.ivy_imports.insert("\u{0275}\u{0275}pipe".to_string());
            self.ivy_imports
                .insert("\u{0275}\u{0275}pipeBind1".to_string());
            self.creation.push(format!(
                "\u{0275}\u{0275}pipe({pipe_slot}, '{}');",
                pipe_name
            ));
            let pipe_var_slot = self.var_count;
            return format!(
                "\u{0275}\u{0275}pipeBind1({pipe_slot}, {pipe_var_slot}, {compiled_base})"
            );
        }

        // No top-level pipe — scan for `(expr | pipe)` sub-expressions and replace them
        let result = replace_nested_pipe_parens(trimmed, self);
        ctx_expr(&result)
    }

    /// Add an `ɵɵadvance()` instruction to the update block if needed.
    fn add_advance(&mut self, _target_slot: u32) {
        self.ivy_imports
            .insert("\u{0275}\u{0275}advance".to_string());
        // For simplicity, always emit advance() (delta = 1)
        // A more sophisticated implementation would compute exact deltas
        self.update.push("\u{0275}\u{0275}advance();".to_string());
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

            // Check if the inner expression has a pipe
            if let Some((base, pipe_name)) = split_top_level_pipe(&inner) {
                // Compile the base expression recursively
                let compiled_base = replace_nested_pipe_parens(&base, gen);
                let compiled_base = ctx_expr(&compiled_base);

                let pipe_slot = gen.slot_index;
                gen.slot_index += 1;
                gen.var_count += 1;
                gen.ivy_imports.insert("\u{0275}\u{0275}pipe".to_string());
                gen.ivy_imports
                    .insert("\u{0275}\u{0275}pipeBind1".to_string());
                gen.creation
                    .push(format!("\u{0275}\u{0275}pipe({pipe_slot}, '{pipe_name}');"));
                let pipe_var_slot = gen.var_count;
                result.push_str(&format!(
                    "\u{0275}\u{0275}pipeBind1({pipe_slot}, {pipe_var_slot}, {compiled_base})"
                ));
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
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Fast path for simple property paths
    if is_simple_property_path(trimmed) {
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
    collect_ctx_rewrites(init_expr, &mut ctx_inserts, &mut remove_ranges, false);

    // Map wrapper offsets back to expression offsets
    let expr_offset = "var __expr = ".len() as u32;
    let mut result = trimmed.to_string();

    // Apply removals first (sorted reverse)
    let mut sorted_removes: Vec<(usize, usize)> = remove_ranges
        .iter()
        .map(|(s, e)| ((s - expr_offset) as usize, (e - expr_offset) as usize))
        .collect();
    sorted_removes.sort_by(|a, b| b.0.cmp(&a.0));
    for (s, e) in &sorted_removes {
        if *s <= result.len() && *e <= result.len() {
            result.replace_range(*s..*e, "");
        }
    }

    // Apply ctx. insertions (sorted reverse)
    let mut sorted_inserts: Vec<usize> = ctx_inserts
        .iter()
        .map(|off| (off - expr_offset) as usize)
        .collect();
    sorted_inserts.sort_unstable();
    sorted_inserts.dedup();
    sorted_inserts.reverse();
    for off in &sorted_inserts {
        if *off <= result.len() {
            result.insert_str(*off, "ctx.");
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
) {
    use oxc_ast::ast::*;
    use oxc_span::GetSpan;

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
        Expression::AssignmentExpression(assign) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &assign.left {
                if !is_builtin(&id.name) {
                    ctx_inserts.push(id.span.start);
                }
            }
            collect_ctx_rewrites(&assign.right, ctx_inserts, remove_ranges, false);
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
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = prop {
                    collect_ctx_rewrites(&p.value, ctx_inserts, remove_ranges, false);
                }
            }
        }
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let ArrayExpressionElement::SpreadElement(spread) = elem {
                    collect_ctx_rewrites(&spread.argument, ctx_inserts, remove_ranges, false);
                } else if !elem.is_elision() {
                    collect_ctx_rewrites(elem.to_expression(), ctx_inserts, remove_ranges, false);
                }
            }
        }
        Expression::TemplateLiteral(tpl) => {
            for expr in &tpl.expressions {
                collect_ctx_rewrites(expr, ctx_inserts, remove_ranges, false);
            }
        }
        _ => {}
    }
}

/// Build a conditional expression for @if chains.
fn build_conditional_expr(
    condition: &str,
    else_ifs: &[(String, String)],
    else_fn: &Option<String>,
) -> String {
    let mut expr = format!("{} ? 0 : ", ctx_expr(condition));

    for (i, (cond, _fn_name)) in else_ifs.iter().enumerate() {
        expr.push_str(&format!("{} ? {} : ", ctx_expr(cond), i + 1));
    }

    if else_fn.is_some() {
        expr.push_str(&format!("{}", else_ifs.len() + 1));
    } else {
        expr.push_str("-1");
    }

    expr
}

/// Format static attributes as an array expression.
fn format_static_attrs(attrs: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = attrs
        .iter()
        .flat_map(|(k, v)| vec![format!("'{k}'"), format!("'{v}'")])
        .collect();
    format!("[{}]", pairs.join(", "))
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
