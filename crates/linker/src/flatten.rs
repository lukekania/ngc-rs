//! Pass B: flatten NgModule references in component `dependencies` arrays.
//!
//! After the npm linker and Pass A have populated the [`ModuleRegistry`] with
//! every known `ɵɵdefineNgModule`, this pass walks all `ɵɵdefineComponent(`
//! calls in the module graph and, for each `dependencies: [...]` array,
//! expands any element that names an NgModule into the module's transitively
//! flattened directive/pipe/component list. Non-NgModule elements
//! (directives, pipes, call expressions, spreads, anything else) are preserved
//! verbatim from the original source text.
//!
//! This is what `ng build` does at AOT time — and removing
//! `ɵɵgetComponentDepsFactory` from runtime behavior is the whole reason we
//! need this pass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpression, ArrayExpressionElement, CallExpression, Declaration,
    ExportDefaultDeclarationKind, Expression, ObjectExpression, ObjectPropertyKind, Program,
    PropertyKey, Statement,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use crate::module_registry::ModuleRegistry;

/// One `dependencies: [...]` array found in an emitted component def whose
/// module references need expansion.
#[derive(Debug)]
struct Replacement {
    start: u32,
    end: u32,
    text: String,
}

/// Walk every module and expand NgModule references in component
/// `dependencies` arrays using `registry`.
///
/// Returns the number of `dependencies` arrays that were actually rewritten.
/// Arrays that contain no NgModule references (or are already fully flat) are
/// left untouched.
pub fn flatten_component_dependencies(
    modules: &mut HashMap<PathBuf, String>,
    registry: &ModuleRegistry,
) -> NgcResult<usize> {
    if registry.is_empty() {
        return Ok(0);
    }
    let paths: Vec<PathBuf> = modules
        .iter()
        .filter(|(_, source)| source.contains("\u{0275}\u{0275}defineComponent"))
        .map(|(path, _)| path.clone())
        .collect();

    let mut rewritten = 0;
    for path in paths {
        let source = match modules.get(&path) {
            Some(s) => s.clone(),
            None => continue,
        };
        if let Some(updated) = flatten_one(&source, &path, registry)? {
            modules.insert(path.clone(), updated);
            rewritten += 1;
            tracing::debug!(path = %path.display(), "flattened component dependencies");
        }
    }
    Ok(rewritten)
}

fn flatten_one(source: &str, path: &Path, registry: &ModuleRegistry) -> NgcResult<Option<String>> {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::mjs()).parse();
    if !parsed.errors.is_empty() {
        return Err(NgcError::LinkerError {
            path: path.to_path_buf(),
            message: format!("parse error: {}", parsed.errors[0]),
        });
    }

    let mut replacements = Vec::new();
    visit_program(&parsed.program, source, registry, &mut replacements);

    if replacements.is_empty() {
        return Ok(None);
    }

    replacements.sort_by_key(|r| std::cmp::Reverse(r.start));
    let mut result = source.to_string();
    for r in &replacements {
        result.replace_range(r.start as usize..r.end as usize, &r.text);
    }
    Ok(Some(result))
}

fn visit_program(
    program: &Program<'_>,
    source: &str,
    registry: &ModuleRegistry,
    out: &mut Vec<Replacement>,
) {
    for stmt in &program.body {
        visit_stmt(stmt, source, registry, out);
    }
}

fn visit_stmt(
    stmt: &Statement<'_>,
    source: &str,
    registry: &ModuleRegistry,
    out: &mut Vec<Replacement>,
) {
    match stmt {
        Statement::ExpressionStatement(s) => visit_expr(&s.expression, source, registry, out),
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    visit_expr(init, source, registry, out);
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(ref d) = export.declaration {
                match d {
                    Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(init) = &declarator.init {
                                visit_expr(init, source, registry, out);
                            }
                        }
                    }
                    Declaration::ClassDeclaration(class) => {
                        visit_class(class, source, registry, out);
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let ExportDefaultDeclarationKind::ClassDeclaration(class) = &export.declaration {
                visit_class(class, source, registry, out);
            }
        }
        Statement::ClassDeclaration(class) => visit_class(class, source, registry, out),
        _ => {}
    }
}

fn visit_class(
    class: &oxc_ast::ast::Class<'_>,
    source: &str,
    registry: &ModuleRegistry,
    out: &mut Vec<Replacement>,
) {
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(prop) => {
                if let Some(ref init) = prop.value {
                    visit_expr(init, source, registry, out);
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(block) => {
                for stmt in &block.body {
                    visit_stmt(stmt, source, registry, out);
                }
            }
            _ => {}
        }
    }
}

fn visit_expr(
    expr: &Expression<'_>,
    source: &str,
    registry: &ModuleRegistry,
    out: &mut Vec<Replacement>,
) {
    match expr {
        Expression::CallExpression(call) => {
            if is_define_component(call) {
                if let Some(obj) = first_object_arg(call) {
                    if let Some(repl) = rewrite_dependencies(obj, source, registry) {
                        out.push(repl);
                    }
                }
            }
            // Walk arguments too — rare, but defineComponent can appear inside
            // helper call wrappers.
            for arg in &call.arguments {
                if let Some(inner) = arg.as_expression() {
                    visit_expr(inner, source, registry, out);
                }
            }
        }
        Expression::AssignmentExpression(a) => visit_expr(&a.right, source, registry, out),
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                visit_expr(e, source, registry, out);
            }
        }
        Expression::ClassExpression(class) => visit_class(class, source, registry, out),
        _ => {}
    }
}

fn is_define_component(call: &CallExpression<'_>) -> bool {
    let name = match &call.callee {
        Expression::Identifier(id) => id.name.as_str(),
        Expression::StaticMemberExpression(m) => m.property.name.as_str(),
        _ => return false,
    };
    name.ends_with("defineComponent")
}

fn first_object_arg<'a>(call: &'a CallExpression<'_>) -> Option<&'a ObjectExpression<'a>> {
    match call.arguments.first()? {
        oxc_ast::ast::Argument::ObjectExpression(obj) => Some(obj.as_ref()),
        _ => None,
    }
}

fn rewrite_dependencies(
    obj: &ObjectExpression<'_>,
    source: &str,
    registry: &ModuleRegistry,
) -> Option<Replacement> {
    let array = find_dependencies_array(obj)?;
    let (new_items, any_expanded) = flatten_array_items(array, source, registry);
    if !any_expanded {
        return None;
    }
    let span = array.span;
    Some(Replacement {
        start: span.start,
        end: span.end,
        text: format!("[{}]", new_items.join(", ")),
    })
}

fn find_dependencies_array<'a>(obj: &'a ObjectExpression<'_>) -> Option<&'a ArrayExpression<'a>> {
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            let key_matches = match &p.key {
                PropertyKey::StaticIdentifier(id) => id.name.as_str() == "dependencies",
                PropertyKey::StringLiteral(s) => s.value.as_str() == "dependencies",
                _ => false,
            };
            if key_matches {
                if let Expression::ArrayExpression(arr) = &p.value {
                    return Some(arr);
                }
            }
        }
    }
    None
}

/// Expand each element of the array. Returns `(items, any_expanded)` where
/// `any_expanded` is true if at least one NgModule identifier was replaced
/// with its flattened exports (including the case of a zero-element flatten,
/// which still counts as a replacement to preserve exact `ng build` semantics).
fn flatten_array_items(
    array: &ArrayExpression<'_>,
    source: &str,
    registry: &ModuleRegistry,
) -> (Vec<String>, bool) {
    let mut items = Vec::new();
    let mut any_expanded = false;
    for element in &array.elements {
        match element {
            ArrayExpressionElement::Identifier(id) => {
                let name = id.name.as_str();
                if registry.is_module(name) {
                    any_expanded = true;
                    for flat in registry.flatten(name) {
                        items.push(flat);
                    }
                } else {
                    items.push(name.to_string());
                }
            }
            ArrayExpressionElement::Elision(_) => {
                // sparse array slot — drop it; Angular treats missing deps as absent
            }
            other => {
                let span = other.span();
                let text = &source[span.start as usize..span.end as usize];
                items.push(text.to_string());
            }
        }
    }
    (items, any_expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> ModuleRegistry {
        let reg = ModuleRegistry::new();
        reg.register(
            "InternalShared",
            vec!["DefaultValueAccessor".into(), "NgControlStatus".into()],
        );
        reg.register(
            "ReactiveFormsModule",
            vec![
                "InternalShared".into(),
                "FormGroupDirective".into(),
                "FormControlName".into(),
            ],
        );
        reg.register("DialogModule", vec!["CdkDialog".into()]);
        reg
    }

    #[test]
    fn flattens_module_and_keeps_standalone_directive() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "class C {} C.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: C, dependencies: [ReactiveFormsModule, MyStandaloneDir] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(out.contains(
            "dependencies: [DefaultValueAccessor, NgControlStatus, FormGroupDirective, FormControlName, MyStandaloneDir]"
        ), "unexpected output: {out}");
    }

    #[test]
    fn no_rewrite_when_array_has_no_modules() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "X.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: X, dependencies: [SomeDir, OtherPipe] });";
        modules.insert(PathBuf::from("/app/x.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 0);
        let out = modules.get(Path::new("/app/x.js")).unwrap();
        assert!(out.contains("dependencies: [SomeDir, OtherPipe]"));
    }

    #[test]
    fn preserves_non_identifier_elements_verbatim() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "Y.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Y, dependencies: [ReactiveFormsModule, ...extraDeps, someFn()] });";
        modules.insert(PathBuf::from("/app/y.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/y.js")).unwrap();
        assert!(out.contains("...extraDeps"), "spread lost: {out}");
        assert!(out.contains("someFn()"), "call lost: {out}");
        assert!(out.contains("FormControlName"), "flatten lost: {out}");
    }

    #[test]
    fn two_components_in_one_file() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "A.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: A, dependencies: [ReactiveFormsModule] });\n\
B.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: B, dependencies: [DialogModule, MyDir] });";
        modules.insert(PathBuf::from("/app/multi.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1); // one file rewritten; both arrays were handled within it
        let out = modules.get(Path::new("/app/multi.js")).unwrap();
        assert!(out.contains(
            "dependencies: [DefaultValueAccessor, NgControlStatus, FormGroupDirective, FormControlName]"
        ));
        assert!(out.contains("dependencies: [CdkDialog, MyDir]"));
    }

    #[test]
    fn empty_registry_skips_work() {
        let registry = ModuleRegistry::new();
        let mut modules = HashMap::new();
        let source =
            "Z.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Z, dependencies: [X] });";
        modules.insert(PathBuf::from("/app/z.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 0);
    }

    #[test]
    fn file_without_define_component_untouched() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/app/plain.js"),
            "export function f() { return 42; }".to_string(),
        );
        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 0);
    }

    #[test]
    fn handles_namespaced_define_component_call() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        // Some linked output uses i0.ɵɵdefineComponent(...)
        let source = "C.\u{0275}cmp = i0.\u{0275}\u{0275}defineComponent({ type: C, dependencies: [DialogModule] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(out.contains("dependencies: [CdkDialog]"));
    }
}
