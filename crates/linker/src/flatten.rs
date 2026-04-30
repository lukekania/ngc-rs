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

use ngc_diagnostics::NgcResult;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpression, ArrayExpressionElement, CallExpression, Declaration,
    ExportDefaultDeclarationKind, Expression, ImportDeclarationSpecifier, ObjectExpression,
    ObjectPropertyKind, Program, PropertyKey, Statement,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use rayon::prelude::*;

use crate::module_registry::ModuleRegistry;
use crate::public_exports::PublicExports;

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
    /// Source specifier string, e.g. `@angular/forms`.
    source: String,
}

/// A brand-new named-import statement we need to add to the file because no
/// existing import has the right source.
#[derive(Debug, Default)]
struct NewImport {
    names: BTreeSet<String>,
}

/// Walk every module and expand NgModule references in component
/// `dependencies` arrays using `registry`.
///
/// Returns the number of files that were actually rewritten.
pub fn flatten_component_dependencies(
    modules: &mut HashMap<PathBuf, String>,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
) -> NgcResult<usize> {
    if registry.is_empty() {
        return Ok(0);
    }

    // Snapshot the (path, source) pairs for every file that contains a
    // ɵɵdefineComponent call so the parallel transform phase owns its inputs
    // and the module map stays immutable during the fan-out.
    let work: Vec<(PathBuf, String)> = modules
        .iter()
        .filter(|(_, source)| source.contains("\u{0275}\u{0275}defineComponent"))
        .map(|(path, source)| (path.clone(), source.clone()))
        .collect();

    let results: Vec<(PathBuf, Option<String>)> = work
        .par_iter()
        .map(|(path, source)| -> NgcResult<(PathBuf, Option<String>)> {
            let updated = flatten_one(source, path, registry, public_exports)?;
            Ok((path.clone(), updated))
        })
        .collect::<NgcResult<Vec<_>>>()?;

    let mut rewritten = 0;
    for (path, maybe_updated) in results {
        if let Some(updated) = maybe_updated {
            modules.insert(path.clone(), updated);
            rewritten += 1;
            tracing::debug!(path = %path.display(), "flattened component dependencies");
        }
    }
    Ok(rewritten)
}

/// Dry-run flatten: walk every `ɵɵdefineComponent` call in `modules` and
/// return the sorted, de-duplicated list of npm specifiers that
/// [`flatten_component_dependencies`] would inject as **brand-new** import
/// statements (i.e. specifiers not already present in any existing
/// named-import in the file).
///
/// Used by the CLI as a pre-scan so the first npm-resolve call can pull in
/// transitive Angular subpaths before flatten actually rewrites the project,
/// eliminating the post-flatten resolution pass on the happy path. Specifiers
/// that flatten would resolve by *extending an existing* same-source import
/// are intentionally excluded — those are already in `bare_specifiers`.
pub fn scan_introduced_specifiers(
    modules: &HashMap<PathBuf, String>,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
) -> Vec<String> {
    if registry.is_empty() {
        return Vec::new();
    }

    let work: Vec<(&PathBuf, &String)> = modules
        .iter()
        .filter(|(_, source)| source.contains("\u{0275}\u{0275}defineComponent"))
        .collect();

    let lists: Vec<Vec<String>> = work
        .par_iter()
        .map(|(_path, source)| collect_introduced_in_file(source, registry, public_exports))
        .collect();

    let mut seen = std::collections::BTreeSet::new();
    for list in lists {
        for spec in list {
            seen.insert(spec);
        }
    }
    seen.into_iter().collect()
}

/// Single-file dry-run helper for [`scan_introduced_specifiers`].
///
/// Mirrors the AST walk that [`flatten_one`] performs but discards the
/// generated `dependencies`-array replacements — only the keys of
/// `new_imports` are returned, since those are the specifiers that flatten
/// would emit a fresh `import { … } from '<spec>';` for.
fn collect_introduced_in_file(
    source: &str,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
) -> Vec<String> {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::mjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Vec::new();
    }
    let mut imports = collect_named_imports(&parsed.program);
    let mut new_imports: HashMap<String, NewImport> = HashMap::new();
    let mut deps_replacements = Vec::new();
    visit_program_deps(
        &parsed.program,
        source,
        registry,
        public_exports,
        &mut imports,
        &mut new_imports,
        &mut deps_replacements,
    );
    new_imports.into_keys().collect()
}

fn flatten_one(
    source: &str,
    path: &Path,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
) -> NgcResult<Option<String>> {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::mjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        // Dependency flattening is a best-effort post-pass. If the source is
        // unparseable (e.g. a file that already took the transform-fallback
        // path in the CLI after an upstream bug), surface a diagnostic and
        // leave the source untouched rather than aborting the whole build.
        // The earlier warning from the transform step already tells the user
        // what broke.
        let msg = if parsed.panicked {
            "parser panicked".to_string()
        } else {
            format!("{}", parsed.errors[0])
        };
        tracing::warn!(
            path = %path.display(),
            "skipping dependency flatten: {msg}"
        );
        return Ok(None);
    }

    let mut imports = collect_named_imports(&parsed.program);
    let mut new_imports: HashMap<String, NewImport> = HashMap::new();

    let mut deps_replacements = Vec::new();
    visit_program_deps(
        &parsed.program,
        source,
        registry,
        public_exports,
        &mut imports,
        &mut new_imports,
        &mut deps_replacements,
    );

    if deps_replacements.is_empty() {
        return Ok(None);
    }

    // Extend existing imports with added names.
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

    // Prepend any brand-new import statements at the top of the file. Grouped
    // by source path. Placed before any existing content so ES module imports
    // stay at the top of the file.
    if !new_imports.is_empty() {
        let mut prefix = String::new();
        let mut sources: Vec<&String> = new_imports.keys().collect();
        sources.sort();
        for src_spec in sources {
            let ni = &new_imports[src_spec];
            if ni.names.is_empty() {
                continue;
            }
            let names: Vec<String> = ni.names.iter().cloned().collect();
            prefix.push_str(&format!(
                "import {{ {} }} from '{}';\n",
                names.join(", "),
                src_spec
            ));
        }
        if !prefix.is_empty() {
            result.insert_str(0, &prefix);
        }
    }

    Ok(Some(result))
}

/// Collect all top-level `import { a, b } from 'x'` statements.
///
/// For each named-import block, record the exact byte span of the `{ ... }`
/// list (inclusive of both braces) by scanning the source around the first
/// specifier rather than guessing based on fixed offsets — whitespace and
/// multi-line formatting would otherwise misplace the replacement range and
/// corrupt the file.
fn collect_named_imports(program: &Program<'_>) -> Vec<NamedImport> {
    let mut out = Vec::new();
    for stmt in &program.body {
        if let Statement::ImportDeclaration(decl) = stmt {
            let Some(specifiers) = &decl.specifiers else {
                continue;
            };
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
            // Brace positions: scan backward from first specifier for the `{`,
            // and forward from last specifier for the matching `}`. Bounded by
            // the import declaration's own span.
            let decl_span = decl.span();
            let Some(list_span) =
                find_brace_list_span(min_start, max_end, decl_span.start, decl_span.end, program)
            else {
                continue;
            };
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

/// Find the brace span `{ … }` containing the named-import specifiers.
///
/// Takes the byte offsets of the first specifier's start (`first_spec_start`)
/// and the last specifier's end (`last_spec_end`), and the import declaration's
/// own start/end as bounds. Walks backward for `{` and forward for `}`.
/// Returns `None` if braces aren't found — caller skips this import (we only
/// touch well-formed `import { … } from '…'` statements).
fn find_brace_list_span(
    first_spec_start: u32,
    last_spec_end: u32,
    decl_start: u32,
    decl_end: u32,
    program: &Program<'_>,
) -> Option<Span> {
    let src = program.source_text.as_bytes();
    let mut i = first_spec_start as usize;
    let lower = decl_start as usize;
    while i > lower {
        i -= 1;
        match src[i] {
            b'{' => {
                let open = i as u32;
                let mut j = last_spec_end as usize;
                let upper = decl_end as usize;
                while j < upper && j < src.len() {
                    if src[j] == b'}' {
                        return Some(Span::new(open, (j + 1) as u32));
                    }
                    j += 1;
                }
                return None;
            }
            // Stop if we hit something that can't be a brace prefix — should
            // only be whitespace between `{` and first specifier normally.
            b'\n' | b'\r' | b'\t' | b' ' | b',' => continue,
            _ => {}
        }
    }
    None
}

fn visit_program_deps(
    program: &Program<'_>,
    source: &str,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
    imports: &mut [NamedImport],
    new_imports: &mut HashMap<String, NewImport>,
    out: &mut Vec<Replacement>,
) {
    for stmt in &program.body {
        visit_stmt(
            stmt,
            source,
            registry,
            public_exports,
            imports,
            new_imports,
            out,
        );
    }
}

fn visit_stmt(
    stmt: &Statement<'_>,
    source: &str,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
    imports: &mut [NamedImport],
    new_imports: &mut HashMap<String, NewImport>,
    out: &mut Vec<Replacement>,
) {
    match stmt {
        Statement::ExpressionStatement(s) => visit_expr(
            &s.expression,
            source,
            registry,
            public_exports,
            imports,
            new_imports,
            out,
        ),
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    visit_expr(
                        init,
                        source,
                        registry,
                        public_exports,
                        imports,
                        new_imports,
                        out,
                    );
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(ref d) = export.declaration {
                match d {
                    Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(init) = &declarator.init {
                                visit_expr(
                                    init,
                                    source,
                                    registry,
                                    public_exports,
                                    imports,
                                    new_imports,
                                    out,
                                );
                            }
                        }
                    }
                    Declaration::ClassDeclaration(class) => {
                        visit_class(
                            class,
                            source,
                            registry,
                            public_exports,
                            imports,
                            new_imports,
                            out,
                        );
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let ExportDefaultDeclarationKind::ClassDeclaration(class) = &export.declaration {
                visit_class(
                    class,
                    source,
                    registry,
                    public_exports,
                    imports,
                    new_imports,
                    out,
                );
            }
        }
        Statement::ClassDeclaration(class) => visit_class(
            class,
            source,
            registry,
            public_exports,
            imports,
            new_imports,
            out,
        ),
        _ => {}
    }
}

fn visit_class(
    class: &oxc_ast::ast::Class<'_>,
    source: &str,
    registry: &ModuleRegistry,
    public_exports: &PublicExports,
    imports: &mut [NamedImport],
    new_imports: &mut HashMap<String, NewImport>,
    out: &mut Vec<Replacement>,
) {
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(prop) => {
                if let Some(ref init) = prop.value {
                    visit_expr(
                        init,
                        source,
                        registry,
                        public_exports,
                        imports,
                        new_imports,
                        out,
                    );
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(block) => {
                for stmt in &block.body {
                    visit_stmt(
                        stmt,
                        source,
                        registry,
                        public_exports,
                        imports,
                        new_imports,
                        out,
                    );
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
    public_exports: &PublicExports,
    imports: &mut [NamedImport],
    new_imports: &mut HashMap<String, NewImport>,
    out: &mut Vec<Replacement>,
) {
    match expr {
        Expression::CallExpression(call) => {
            if is_define_component(call) {
                if let Some(obj) = first_object_arg(call) {
                    if let Some(repl) = rewrite_dependencies(
                        obj,
                        source,
                        registry,
                        public_exports,
                        imports,
                        new_imports,
                    ) {
                        out.push(repl);
                    }
                }
            }
            for arg in &call.arguments {
                if let Some(inner) = arg.as_expression() {
                    visit_expr(
                        inner,
                        source,
                        registry,
                        public_exports,
                        imports,
                        new_imports,
                        out,
                    );
                }
            }
        }
        Expression::AssignmentExpression(a) => visit_expr(
            &a.right,
            source,
            registry,
            public_exports,
            imports,
            new_imports,
            out,
        ),
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                visit_expr(
                    e,
                    source,
                    registry,
                    public_exports,
                    imports,
                    new_imports,
                    out,
                );
            }
        }
        Expression::ClassExpression(class) => visit_class(
            class,
            source,
            registry,
            public_exports,
            imports,
            new_imports,
            out,
        ),
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
    public_exports: &PublicExports,
    imports: &mut [NamedImport],
    new_imports: &mut HashMap<String, NewImport>,
) -> Option<Replacement> {
    let array = find_dependencies_array(obj)?;
    let (new_items, any_expanded) = flatten_array_items(
        array,
        source,
        registry,
        public_exports,
        imports,
        new_imports,
    );
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
    public_exports: &PublicExports,
    imports: &mut [NamedImport],
    new_imports: &mut HashMap<String, NewImport>,
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
                    for new_name in &flat {
                        // Skip underscore-prefixed names — these are JS-private
                        // by convention and typically not publicly exported from
                        // any npm package (e.g. `_EmptyOutletComponent` from
                        // `@angular/router`).
                        if new_name.starts_with('_') {
                            continue;
                        }
                        // If the name is already in scope via any existing
                        // import (original or already-scheduled addition), just
                        // emit it — no import rewrite needed.
                        if any_import_has(imports, new_name) {
                            push_unique(&mut items, &mut seen, new_name.clone());
                            continue;
                        }
                        // Find the npm specifier that publicly exports this
                        // name. If none is known, we cannot safely add it —
                        // including it in deps would bind to `undefined` and
                        // trigger NG0919. Drop it silently (matches what ng
                        // build effectively does when a directive is not
                        // reachable).
                        let Some(spec) = public_exports.specifier_for(new_name) else {
                            tracing::debug!(
                                name = %new_name,
                                "flatten: dropping directive — no public npm export found"
                            );
                            continue;
                        };
                        // Prefer extending an existing same-source import.
                        if let Some(idx) = find_import_by_source(imports, &spec) {
                            imports[idx].additions.insert(new_name.clone());
                        } else {
                            new_imports
                                .entry(spec)
                                .or_default()
                                .names
                                .insert(new_name.clone());
                        }
                        push_unique(&mut items, &mut seen, new_name.clone());
                    }
                } else {
                    push_unique(&mut items, &mut seen, name.to_string());
                }
            }
            ArrayExpressionElement::Elision(_) => {}
            other => {
                let span = other.span();
                let text = &source[span.start as usize..span.end as usize];
                items.push(text.to_string());
            }
        }
    }
    (items, any_expanded)
}

fn find_import_by_source(imports: &[NamedImport], source: &str) -> Option<usize> {
    imports.iter().position(|i| i.source == source)
}

fn any_import_has(imports: &[NamedImport], name: &str) -> bool {
    imports
        .iter()
        .any(|i| i.existing.contains(name) || i.additions.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_public_exports() -> PublicExports {
        // Populate with all the names the tests expand to, from plausible
        // specifier origins. Tests that specifically want to exercise the
        // "name not publicly exported" case use a separate, empty registry.
        let pe = PublicExports::new();
        let forms = "/project/node_modules/@angular/forms/fesm2022/forms.mjs";
        let exports = "export { DefaultValueAccessor, NgControlStatus, FormGroupDirective, FormControlName, NgModel, \u{0275}NgNoValidate };";
        pe.scan_file(exports, Path::new(forms));
        let router = "/project/node_modules/@angular/router/fesm2022/router.mjs";
        pe.scan_file(
            "export { RouterOutlet, RouterLink, RouterLinkActive };",
            Path::new(router),
        );
        let dialog = "/project/node_modules/@angular/cdk/fesm2022/dialog.mjs";
        pe.scan_file("export { CdkDialogContainer };", Path::new(dialog));
        let portal = "/project/node_modules/@angular/cdk/fesm2022/portal.mjs";
        pe.scan_file("export { CdkPortal, CdkPortalOutlet };", Path::new(portal));
        let cdk_dialog_for_cdkdialog = "/project/node_modules/@angular/cdk/fesm2022/dialog.mjs";
        pe.scan_file("export { CdkDialog };", Path::new(cdk_dialog_for_cdkdialog));
        let x_mod = "/project/node_modules/x/fesm2022/x.mjs";
        pe.scan_file("export { SomeDir, OtherPipe };", Path::new(x_mod));
        pe
    }

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

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
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

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
        assert_eq!(rewritten, 0);
    }

    #[test]
    fn preserves_non_identifier_elements_verbatim() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "import { ReactiveFormsModule } from '@angular/forms';\n\
Y.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Y, dependencies: [ReactiveFormsModule, ...extraDeps, someFn()] });";
        modules.insert(PathBuf::from("/app/y.js"), source.to_string());

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
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

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
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

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
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
        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
        assert_eq!(rewritten, 0);
    }

    #[test]
    fn handles_namespaced_define_component_call() {
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "import { DialogModule } from '@angular/cdk/dialog';\n\
C.\u{0275}cmp = i0.\u{0275}\u{0275}defineComponent({ type: C, dependencies: [DialogModule] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
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

        let rewritten =
            flatten_component_dependencies(&mut modules, &reg, &make_public_exports()).unwrap();
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

        let rewritten =
            flatten_component_dependencies(&mut modules, &reg, &make_public_exports()).unwrap();
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

        let _ = flatten_component_dependencies(&mut modules, &reg, &make_public_exports()).unwrap();
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(out.contains("\u{0275}NgNoValidate"));
        assert!(out.contains("FormGroupDirective"));
    }

    #[test]
    fn adds_new_import_when_module_not_imported_in_file() {
        // Edge case: the dependencies array names a module that the file doesn't
        // import directly. Now that we know each directive's public npm
        // specifier (via PublicExports), we can emit a brand-new import
        // statement so the flattened names are actually in scope.
        let registry = make_registry();
        let mut modules = HashMap::new();
        let source = "Q.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: Q, dependencies: [ReactiveFormsModule] });";
        modules.insert(PathBuf::from("/app/q.js"), source.to_string());

        let rewritten =
            flatten_component_dependencies(&mut modules, &registry, &make_public_exports())
                .unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/q.js")).unwrap();
        // deps array got flattened
        assert!(out.contains("FormControlName"));
        // A new import statement is prepended for the flattened names, from
        // the correct source package inferred via PublicExports.
        assert!(
            out.contains("from '@angular/forms'"),
            "expected injected import, got: {out}"
        );
    }

    #[test]
    fn extends_multiline_import_block_correctly() {
        // Regression: early implementations assumed `{ A, B }` fixed-offset
        // braces and corrupted multi-line imports (which Prettier produces
        // for long lists). The file would end up with a truncated import
        // and stale code bleeding into the rewritten span — observed at
        // runtime as a dialog rendering its backdrop but not its container.
        let reg = make_registry();
        let mut modules = HashMap::new();
        let source = "import {\n  ReactiveFormsModule,\n  FormBuilder,\n  FormGroup,\n  Validators,\n} from '@angular/forms';\n\
class C {}\n\
C.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: C, dependencies: [ReactiveFormsModule] });";
        modules.insert(PathBuf::from("/app/m.js"), source.to_string());

        let rewritten =
            flatten_component_dependencies(&mut modules, &reg, &make_public_exports()).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/m.js")).unwrap();
        // The defineComponent call must still be intact (no truncation).
        assert!(
            out.contains("C.\u{0275}cmp = \u{0275}\u{0275}defineComponent"),
            "defineComponent corrupted: {out}"
        );
        assert!(out.contains("class C {}"), "class C corrupted: {out}");
        // All original import names must still be present exactly once,
        // checked as whole tokens (avoid `FormGroup` matching `FormGroupDirective`).
        let import_section = out.split("} from '@angular/forms'").next().unwrap();
        let names: Vec<&str> = import_section
            .trim_start_matches("import {")
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        for needed in [
            "ReactiveFormsModule",
            "FormBuilder",
            "FormGroup",
            "Validators",
        ] {
            let count = names.iter().filter(|n| **n == needed).count();
            assert_eq!(count, 1, "{needed} miscounted in: {import_section}");
        }
    }

    #[test]
    fn drops_name_without_known_public_export() {
        // Edge case: a flattened name has no entry in PublicExports — we must
        // not include it in deps (would be undefined at runtime) and must not
        // fabricate an import (we don't know where from).
        let reg = ModuleRegistry::new();
        reg.register(
            "MysteryModule",
            vec!["KnownDir".into(), "UnknownDir".into()],
        );

        // PublicExports that knows about KnownDir but not UnknownDir.
        let pe = PublicExports::new();
        pe.scan_file(
            "export { KnownDir };",
            Path::new("/proj/node_modules/pkg/fesm2022/pkg.mjs"),
        );

        let mut modules = HashMap::new();
        let source = "C.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: C, dependencies: [MysteryModule] });";
        modules.insert(PathBuf::from("/app/c.js"), source.to_string());

        let rewritten = flatten_component_dependencies(&mut modules, &reg, &pe).unwrap();
        assert_eq!(rewritten, 1);
        let out = modules.get(Path::new("/app/c.js")).unwrap();
        assert!(out.contains("KnownDir"));
        assert!(!out.contains("UnknownDir"), "UnknownDir leaked: {out}");
    }

    #[test]
    fn adds_to_different_import_when_directive_lives_in_different_subpath() {
        // Real-world case: DialogModule re-exports PortalModule (whose exports
        // live in @angular/cdk/portal), but the project only imports DialogModule
        // from @angular/cdk/dialog. Flatten must add CdkPortal/CdkPortalOutlet
        // via a NEW import from @angular/cdk/portal, not the existing
        // @angular/cdk/dialog one (which doesn't export those names).
        let reg = ModuleRegistry::new();
        reg.register(
            "PortalModule",
            vec!["CdkPortal".into(), "CdkPortalOutlet".into()],
        );
        reg.register(
            "DialogModule",
            vec!["PortalModule".into(), "CdkDialogContainer".into()],
        );

        let pe = PublicExports::new();
        pe.scan_file(
            "export { CdkPortal, CdkPortalOutlet };",
            Path::new("/proj/node_modules/@angular/cdk/fesm2022/portal.mjs"),
        );
        pe.scan_file(
            "export { CdkDialogContainer, DialogModule };",
            Path::new("/proj/node_modules/@angular/cdk/fesm2022/dialog.mjs"),
        );

        let mut modules = HashMap::new();
        let source = "import { DialogModule } from '@angular/cdk/dialog';\n\
D.\u{0275}cmp = \u{0275}\u{0275}defineComponent({ type: D, dependencies: [DialogModule] });";
        modules.insert(PathBuf::from("/app/d.js"), source.to_string());

        flatten_component_dependencies(&mut modules, &reg, &pe).unwrap();
        let out = modules.get(Path::new("/app/d.js")).unwrap();
        assert!(out.contains("dependencies: [CdkPortal, CdkPortalOutlet, CdkDialogContainer]"));
        // CdkDialogContainer was added to the existing @angular/cdk/dialog import.
        let dialog_line = out
            .lines()
            .find(|l| l.contains("from '@angular/cdk/dialog'"))
            .expect("dialog import");
        assert!(dialog_line.contains("CdkDialogContainer"), "{dialog_line}");
        assert!(
            !dialog_line.contains("CdkPortal"),
            "CdkPortal must NOT be added to dialog import: {dialog_line}"
        );
        // CdkPortal + CdkPortalOutlet were added via a NEW import from
        // @angular/cdk/portal.
        let portal_line = out
            .lines()
            .find(|l| l.contains("from '@angular/cdk/portal'"))
            .expect("portal import injected");
        assert!(portal_line.contains("CdkPortal"));
        assert!(portal_line.contains("CdkPortalOutlet"));
    }

    #[test]
    fn test_flatten_one_skips_unparseable_source() {
        // Regression guard for GH #81. When the CLI's transform-fallback
        // path forwards a file that oxc could not parse, this pass must not
        // surface a second `parse error: Expected \`,\` or \`)\` but found \`:\``
        // — it should log a warning and leave the source untouched.
        let broken = r#"export class Broken {
  static \u{0275}cmp = \u{0275}\u{0275}defineComponent({
    styles: [`.a[_ngcontent-%COMP%]{ color: red; }`[_ngcontent-%COMP%], `.b`]
  });
}"#;
        let registry = ModuleRegistry::new();
        registry.register("AnyModule", vec!["X".into()]);
        let public_exports = PublicExports::new();
        let result = flatten_one(
            broken,
            Path::new("/project/src/broken.component.ts"),
            &registry,
            &public_exports,
        );
        let none = result.expect("unparseable source must not error");
        assert!(
            none.is_none(),
            "unparseable source should be skipped, not rewritten"
        );
    }
}
