//! Pass B: flatten NgModule references in component `dependencies` arrays.
//!
//! After the npm linker and Pass A have populated the [`ModuleRegistry`] with
//! every known `ɵɵdefineNgModule`, this pass walks all `ɵɵdefineComponent(`
//! calls in the module graph and, for each `dependencies: [...]` array,
//! expands any element that names an NgModule into the module's transitively
//! flattened directive/pipe/component list.
//!
//! Because the expanded directive identifiers (e.g. `NgControlStatus`,
//! `FormGroupDirective`) are not normally imported into the project file —
//! the source typically only imports the wrapping module like
//! `import { ReactiveFormsModule } from '@angular/forms'` — this pass also
//! **extends the file's existing named imports** so the new identifiers are
//! actually defined at runtime. Without that step, the flattened deps array
//! would reference dangling names and Angular would `ReferenceError` during
//! component definition (white-screen-of-death).
//!
//! The per-module source of truth is the project file's *own* import
//! statement: if the file imports `{ ReactiveFormsModule } from '@angular/forms'`,
//! we add `NgControlStatus, FormGroupDirective, …` to that same brace list.
//! This sidesteps the npm-package-vs-subpath problem because we mirror what
//! the source file already chose.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpression, ArrayExpressionElement, CallExpression, Declaration,
    ExportDefaultDeclarationKind, Expression, ImportDeclarationSpecifier, ObjectExpression,
    ObjectPropertyKind, Program, PropertyKey, Statement,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};

use crate::module_registry::ModuleRegistry;

/// One textual replacement to apply to the source.
#[derive(Debug)]
struct Replacement {
    start: u32,
    end: u32,
    text: String,
}

/// Information about a single named-import statement in the file.
///
/// We track only `import { a, b } from 'x'` style — the form the template
/// compiler emits for project files. Namespace and default imports are
/// recorded for name lookup but cannot be extended in place.
#[derive(Debug)]
struct NamedImport {
    /// Span of the brace list including braces, e.g. `{ A, B }`.
    list_span: Span,
    /// Existing imported local names.
    existing: BTreeSet<String>,
    /// New names to add, accumulated during the deps walk.
    additions: BTreeSet<String>,
    /// Source path string, e.g. `@angular/forms`. Currently unused in the code
    /// path — we attribute new names via the owning-import index — but kept
    /// for future use (e.g. adding a new import statement when no owning import
    /// exists) and for debugging.
    #[allow(dead_code)]
    source: String,
}

/// Walk every module and expand NgModule references in component
/// `dependencies` arrays using `registry`.
///
/// Returns the number of files that were actually rewritten.
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

    let mut imports = collect_named_imports(&parsed.program);

    let mut deps_replacements = Vec::new();
    visit_program_deps(
        &parsed.program,
        source,
        registry,
        &mut imports,
        &mut deps_replacements,
    );

    if deps_replacements.is_empty() {
        return Ok(None);
    }

    // Build replacements for each import that gained additions.
    let mut all_replacements: Vec<Replacement> = deps_replacements;
    for imp in &imports {
        let truly_new: Vec<&String> = imp
            .additions
            .iter()
            .filter(|n| !imp.existing.contains(n.as_str()))
            .collect();
        if truly_new.is_empty() {
            continue;
        }
        let mut all_names: Vec<String> = imp.existing.iter().cloned().collect();
        for n in truly_new {
            all_names.push(n.clone());
        }
        let text = format!("{{ {} }}", all_names.join(", "));
        all_replacements.push(Replacement {
            start: imp.list_span.start,
            end: imp.list_span.end,
            text,
        });
    }

    all_replacements.sort_by_key(|r| std::cmp::Reverse(r.start));
    let mut result = source.to_string();
    for r in &all_replacements {
        result.replace_range(r.start as usize..r.end as usize, &r.text);
    }
    Ok(Some(result))
}

/// Collect all top-level `import { a, b } from 'x'` statements.
fn collect_named_imports(program: &Program<'_>) -> Vec<NamedImport> {
    let mut out = Vec::new();
    for stmt in &program.body {
        if let Statement::ImportDeclaration(decl) = stmt {
            let Some(specifiers) = &decl.specifiers else {
                continue;
            };
            // Determine if there's at least one named import (vs. only namespace/default)
            let mut has_named = false;
            let mut existing = BTreeSet::new();
            let mut min_start = u32::MAX;
            let mut max_end = 0u32;
            for spec in specifiers {
                if let ImportDeclarationSpecifier::ImportSpecifier(s) = spec {
                    has_named = true;
                    existing.insert(s.local.name.to_string());
                    let sp = s.span();
                    if sp.start < min_start {
                        min_start = sp.start;
                    }
                    if sp.end > max_end {
                        max_end = sp.end;
                    }
                }
            }
            if !has_named {
                continue;
            }
            // Find the brace span: scan source to widen min_start..max_end to include braces.
            // We use the parser-provided endpoints; the surrounding braces sit just outside
            // the first/last specifier. Caller widens by ±1 byte to swallow `{` and `}`.
            // To be safe, use the declaration's source range minus the from-clause.
            // Simpler and reliable: construct the new brace block as `{ ... }` but
            // replace exactly the existing brace block, located by scanning.
            let list_span = Span::new(min_start.saturating_sub(2), max_end + 2);
            out.push(NamedImport {
                list_span,
                existing,
                additions: BTreeSet::new(),
                source: decl.source.value.to_string(),
            });
        }
    }
    out
}

fn visit_program_deps(
    program: &Program<'_>,
    source: &str,
    registry: &ModuleRegistry,
    imports: &mut [NamedImport],
    out: &mut Vec<Replacement>,
) {
    for stmt in &program.body {
        visit_stmt(stmt, source, registry, imports, out);
    }
}

fn visit_stmt(
    stmt: &Statement<'_>,
    source: &str,
    registry: &ModuleRegistry,
    imports: &mut [NamedImport],
    out: &mut Vec<Replacement>,
) {
    match stmt {
        Statement::ExpressionStatement(s) => {
            visit_expr(&s.expression, source, registry, imports, out)
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    visit_expr(init, source, registry, imports, out);
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(ref d) = export.declaration {
                match d {
                    Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(init) = &declarator.init {
                                visit_expr(init, source, registry, imports, out);
                            }
                        }
                    }
                    Declaration::ClassDeclaration(class) => {
                        visit_class(class, source, registry, imports, out);
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let ExportDefaultDeclarationKind::ClassDeclaration(class) = &export.declaration {
                visit_class(class, source, registry, imports, out);
            }
        }
        Statement::ClassDeclaration(class) => visit_class(class, source, registry, imports, out),
        _ => {}
    }
}

fn visit_class(
    class: &oxc_ast::ast::Class<'_>,
    source: &str,
    registry: &ModuleRegistry,
    imports: &mut [NamedImport],
    out: &mut Vec<Replacement>,
) {
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(prop) => {
                if let Some(ref init) = prop.value {
                    visit_expr(init, source, registry, imports, out);
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(block) => {
                for stmt in &block.body {
                    visit_stmt(stmt, source, registry, imports, out);
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
    imports: &mut [NamedImport],
    out: &mut Vec<Replacement>,
) {
    match expr {
        Expression::CallExpression(call) => {
            if is_define_component(call) {
                if let Some(obj) = first_object_arg(call) {
                    if let Some(repl) = rewrite_dependencies(obj, source, registry, imports) {
                        out.push(repl);
                    }
                }
            }
            for arg in &call.arguments {
                if let Some(inner) = arg.as_expression() {
                    visit_expr(inner, source, registry, imports, out);
                }
            }
        }
        Expression::AssignmentExpression(a) => visit_expr(&a.right, source, registry, imports, out),
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                visit_expr(e, source, registry, imports, out);
            }
        }
        Expression::ClassExpression(class) => visit_class(class, source, registry, imports, out),
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
    imports: &mut [NamedImport],
) -> Option<Replacement> {
    let array = find_dependencies_array(obj)?;
    let (new_items, any_expanded) = flatten_array_items(array, source, registry, imports);
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

/// Expand each element of the array. Returns `(items, any_expanded)`.
///
/// Deduplication happens at the *array* level, not per-module: two
/// flattened modules that re-export the same internal module (e.g.
/// `FormsModule` and `ReactiveFormsModule` both re-exporting the internal
/// forms-shared module) must not emit the shared directives twice, or Angular
/// will throw `NG0919 — Cannot read @Component metadata` at runtime.
///
/// As a side effect, schedules import additions on `imports` for any directive
/// names that the file does not yet have in scope.
fn flatten_array_items(
    array: &ArrayExpression<'_>,
    source: &str,
    registry: &ModuleRegistry,
    imports: &mut [NamedImport],
) -> (Vec<String>, bool) {
    let mut items: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut any_expanded = false;

    let push_unique = |items: &mut Vec<String>, seen: &mut BTreeSet<String>, value: String| {
        if seen.insert(value.clone()) {
            items.push(value);
        }
    };

    for element in &array.elements {
        match element {
            ArrayExpressionElement::Identifier(id) => {
                let name = id.name.as_str();
                if registry.is_module(name) {
                    any_expanded = true;
                    let flat = registry.flatten(name);
                    let owner_idx = find_import_owning(imports, name);
                    for new_name in &flat {
                        // Skip identifiers that start with an underscore — by JS
                        // convention they are package-private. Some Angular
                        // modules (notably `RouterModule`) list such classes in
                        // `ɵmod.exports` for their own internal resolution, but
                        // the classes are *not* publicly exported from the npm
                        // package. Adding them to a project file's import would
                        // bind to `undefined` at runtime → `NG0919 — Cannot
                        // read @Component metadata`. The ɵ-prefix convention
                        // (e.g. `ɵNgNoValidate`) *is* publicly exported and
                        // passes through normally.
                        if new_name.starts_with('_') {
                            continue;
                        }
                        push_unique(&mut items, &mut seen, new_name.clone());
                        if let Some(idx) = owner_idx {
                            if !any_import_has(imports, new_name) {
                                imports[idx].additions.insert(new_name.clone());
                            }
                        }
                    }
                } else {
                    push_unique(&mut items, &mut seen, name.to_string());
                }
            }
            ArrayExpressionElement::Elision(_) => {}
            other => {
                let span = other.span();
                let text = &source[span.start as usize..span.end as usize];
                // Non-identifier elements (spreads, calls, etc.) keep verbatim
                // text and don't participate in dedup.
                items.push(text.to_string());
            }
        }
    }
    (items, any_expanded)
}

fn find_import_owning(imports: &[NamedImport], name: &str) -> Option<usize> {
    imports.iter().position(|i| i.existing.contains(name))
}

fn any_import_has(imports: &[NamedImport], name: &str) -> bool {
    imports
        .iter()
        .any(|i| i.existing.contains(name) || i.additions.contains(name))
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
    fn flattens_module_and_extends_existing_import() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "import { ReactiveFormsModule, FormBuilder } from '@angular/forms';\n\
import { MyStandaloneDir } from './my-dir';\n\
class C {}\n\
C.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: C, dependencies: [ReactiveFormsModule, MyStandaloneDir] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(
            out.contains(
                "dependencies: [DefaultValueAccessor, NgControlStatus, FormGroupDirective, FormControlName, MyStandaloneDir]"
            ),
            "deps array unexpected: {out}"
        );
        // The import statement should now also bring in the directive names.
        for needed in [
            "DefaultValueAccessor",
            "NgControlStatus",
            "FormGroupDirective",
            "FormControlName",
        ] {
            assert!(out.contains(needed), "directive {needed} missing: {out}");
        }
        // Specifically, the @angular/forms import line should now include them.
        assert!(
            out.contains("from '@angular/forms'"),
            "forms import missing: {out}"
        );
        // The @angular/forms import should contain the directive names in its brace list.
        let forms_line = out
            .lines()
            .find(|l| l.contains("from '@angular/forms'"))
            .expect("forms import line");
        for needed in [
            "ReactiveFormsModule",
            "FormBuilder",
            "DefaultValueAccessor",
            "NgControlStatus",
            "FormGroupDirective",
            "FormControlName",
        ] {
            assert!(
                forms_line.contains(needed),
                "{needed} not in forms import: {forms_line}"
            );
        }
    }

    #[test]
    fn no_rewrite_when_array_has_no_modules() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "import { SomeDir, OtherPipe } from 'x';\n\
X.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: X, dependencies: [SomeDir, OtherPipe] });";
        modules.insert(PathBuf::from("/app/x.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 0);
    }

    #[test]
    fn preserves_non_identifier_elements_verbatim() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "import { ReactiveFormsModule } from '@angular/forms';\n\
Y.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Y, dependencies: [ReactiveFormsModule, ...extraDeps, someFn()] });";
        modules.insert(PathBuf::from("/app/y.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/y.js")).unwrap();
        assert!(out.contains("...extraDeps"));
        assert!(out.contains("someFn()"));
        assert!(out.contains("FormControlName"));
    }

    #[test]
    fn does_not_duplicate_already_imported_directive() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        // FormControlName is already imported directly for some other use.
        let source = "import { ReactiveFormsModule, FormControlName } from '@angular/forms';\n\
Z.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Z, dependencies: [ReactiveFormsModule] });";
        modules.insert(PathBuf::from("/app/z.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/z.js")).unwrap();
        let forms_line = out
            .lines()
            .find(|l| l.contains("from '@angular/forms'"))
            .expect("forms import line");
        // FormControlName must appear exactly once in the brace list.
        let count = forms_line.matches("FormControlName").count();
        assert_eq!(count, 1, "duplicate import: {forms_line}");
    }

    #[test]
    fn empty_registry_skips_work() {
        let registry = ModuleRegistry::new();
        let mut modules = HashMap::new();
        let source = "import { X } from 'x';\n\
Z.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Z, dependencies: [X] });";
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
        let source = "import { DialogModule } from '@angular/cdk/dialog';\n\
C.\u{0275}cmp = i0.\u{0275}\u{0275}defineComponent({ type: C, dependencies: [DialogModule] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(out.contains("dependencies: [CdkDialog]"));
        let cdk_line = out
            .lines()
            .find(|l| l.contains("from '@angular/cdk/dialog'"))
            .expect("cdk import line");
        assert!(cdk_line.contains("CdkDialog"), "{cdk_line}");
    }

    #[test]
    fn dedups_across_sibling_modules_sharing_internal_exports() {
        // Mirrors the real-world case: FormsModule and ReactiveFormsModule
        // both re-export the same shared internal module. Without
        // cross-module dedup we'd emit every shared directive twice, which
        // Angular rejects with NG0919.
        let reg = ModuleRegistry::new();
        reg.register(
            "InternalShared",
            vec!["DefaultValueAccessor".into(), "NgControlStatus".into()],
        );
        reg.register(
            "FormsModule",
            vec!["InternalShared".into(), "NgModel".into()],
        );
        reg.register(
            "ReactiveFormsModule",
            vec!["InternalShared".into(), "FormGroupDirective".into()],
        );

        let mut modules = HashMap::new();
        let source = "import { FormsModule, ReactiveFormsModule } from '@angular/forms';\n\
C.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: C, dependencies: [FormsModule, ReactiveFormsModule] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &reg).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/c.js")).unwrap();

        // Extract deps array content.
        let start = out.find("dependencies: [").unwrap() + "dependencies: [".len();
        let end = out[start..].find(']').unwrap() + start;
        let arr = &out[start..end];
        let items: Vec<&str> = arr.split(',').map(|s| s.trim()).collect();
        // Each name appears exactly once.
        for needed in [
            "DefaultValueAccessor",
            "NgControlStatus",
            "NgModel",
            "FormGroupDirective",
        ] {
            let count = items.iter().filter(|x| **x == needed).count();
            assert_eq!(count, 1, "{needed} appeared {count} times in {arr}");
        }
    }

    #[test]
    fn skips_underscore_prefixed_private_exports() {
        // Mirrors the RouterModule case: its ɵmod.exports list contains
        // `_EmptyOutletComponent`, an internal class not publicly exported
        // from @angular/router. We must not emit such names in project
        // dependency arrays — they bind to `undefined` and throw NG0919.
        let reg = ModuleRegistry::new();
        reg.register(
            "RouterModule",
            vec![
                "RouterOutlet".into(),
                "RouterLink".into(),
                "RouterLinkActive".into(),
                "_EmptyOutletComponent".into(),
            ],
        );

        let mut modules = HashMap::new();
        let source = "import { RouterModule } from '@angular/router';\n\
S.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: S, dependencies: [RouterModule] });";
        modules.insert(PathBuf::from("/app/s.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &reg).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/s.js")).unwrap();
        assert!(out.contains("dependencies: [RouterOutlet, RouterLink, RouterLinkActive]"));
        assert!(!out.contains("_EmptyOutletComponent"));
        // The import line should include the three public directives but not the
        // private one.
        let router_line = out
            .lines()
            .find(|l| l.contains("from '@angular/router'"))
            .expect("router import line");
        for needed in ["RouterOutlet", "RouterLink", "RouterLinkActive"] {
            assert!(router_line.contains(needed), "{router_line}");
        }
        assert!(
            !router_line.contains("_EmptyOutletComponent"),
            "{router_line}"
        );
    }

    #[test]
    fn keeps_theta_prefixed_public_exports() {
        // ɵ-prefix is the Angular convention for "internal but still publicly
        // exported" (e.g. `ɵNgNoValidate` from @angular/forms). We must not
        // filter these — they ARE importable.
        let reg = ModuleRegistry::new();
        reg.register(
            "ReactiveFormsModule",
            vec!["\u{0275}NgNoValidate".into(), "FormGroupDirective".into()],
        );

        let mut modules = HashMap::new();
        let source = "import { ReactiveFormsModule } from '@angular/forms';\n\
C.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: C, dependencies: [ReactiveFormsModule] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let _ = flatten_component_dependencies(&mut modules, &reg).unwrap();
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(out.contains("\u{0275}NgNoValidate"));
        assert!(out.contains("FormGroupDirective"));
    }

    #[test]
    fn skips_import_extension_when_module_not_imported_in_file() {
        // Edge case: the dependencies array names a module that the file doesn't
        // import directly. We still flatten the array but we cannot guess where
        // to import the directives from. Currently we only extend imports we can
        // attribute; the bundler may leave the names unresolved — caller's
        // responsibility to ensure the source brings the module in.
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "Q.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Q, dependencies: [ReactiveFormsModule] });";
        modules.insert(PathBuf::from("/app/q.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &registry).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/q.js")).unwrap();
        // deps array got flattened
        assert!(out.contains("FormControlName"));
        // no import lines were touched (none existed)
        assert!(!out.contains("from '@angular/forms'"));
    }
}
