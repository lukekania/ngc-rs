//! Registry of NgModule classes and their exported directive/pipe/component lists.
//!
//! Populated from both npm `ɵɵngDeclareNgModule` calls (during npm linking) and
//! `ɵɵdefineNgModule` calls (project AOT output). Used by the post-link pass that
//! flattens standalone-component `imports` arrays at build time, mirroring what
//! `ng build` does.
//!
//! ## Why
//!
//! Modern Angular apps are standalone-first, but npm packages still ship their
//! directives bundled into NgModules (e.g. `ReactiveFormsModule` from
//! `@angular/forms`). When a standalone component does
//! `imports: [ReactiveFormsModule]`, Angular's official compiler walks the
//! module's `ɵmod.exports` transitively and emits a flat directive class list
//! on the component def. Doing this at compile time avoids the runtime
//! `ɵɵgetComponentDepsFactory` helper, which has known instantiation-order bugs
//! that surface as `NG01050` for reactive forms inside dialogs.
//!
//! ## Lifecycle
//!
//! - **Population is split** between npm linking (`ng_module::transform`) and a
//!   post-pass that scans every module for `ɵɵdefineNgModule(`. This is why
//!   transitive flattening is *lazy* — both populations must complete before
//!   any flatten lookup is correct.
//! - **Per-invocation** today; watch mode (v0.8) will need invalidation hooks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rayon::prelude::*;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    CallExpression, Declaration, ExportDefaultDeclarationKind, Expression, ObjectExpression,
    Program, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::metadata;

/// Maps NgModule class names to their (transitively flattened) exported
/// directive/pipe/component class names.
///
/// All keys/values are local source identifiers as they appear in the file
/// where the NgModule was declared. Cross-file linking happens via the bundler
/// after this stage.
#[derive(Debug, Default)]
pub struct ModuleRegistry {
    /// Class name -> direct exports (in source order, possibly containing other
    /// module names that need transitive expansion).
    raw_exports: RwLock<HashMap<String, Vec<String>>>,
    /// Set of class names known to be NgModules (i.e., have been registered).
    is_module: RwLock<HashSet<String>>,
    /// Memoized result of [`flatten`]. Cleared implicitly between builds since
    /// each build constructs a fresh registry.
    flat_cache: RwLock<HashMap<String, Vec<String>>>,
}

impl ModuleRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an NgModule with its direct (un-flattened) exports list.
    ///
    /// Idempotent for identical re-registration (Pass A may re-visit a module
    /// already registered by the npm link pass). If the module name is already
    /// present with a *different* exports list, the later registration wins
    /// and a `tracing::warn!` is emitted — this handles the rare case where a
    /// project file shadows an npm package's NgModule.
    pub fn register(&self, name: &str, exports: Vec<String>) {
        let mut changed = true;
        {
            let mut raw = match self.raw_exports.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match raw.get(name) {
                Some(existing) if existing == &exports => {
                    changed = false;
                }
                Some(_) => {
                    tracing::warn!(
                        module = name,
                        "NgModule registered with conflicting exports; later registration wins"
                    );
                    raw.insert(name.to_string(), exports);
                }
                None => {
                    raw.insert(name.to_string(), exports);
                }
            }
        }
        {
            let mut set = match self.is_module.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            set.insert(name.to_string());
        }
        if changed {
            if let Ok(mut cache) = self.flat_cache.write() {
                cache.clear();
            }
        }
    }

    /// Whether the given class name is a known NgModule.
    pub fn is_module(&self, name: &str) -> bool {
        match self.is_module.read() {
            Ok(g) => g.contains(name),
            Err(p) => p.into_inner().contains(name),
        }
    }

    /// Number of registered NgModules.
    pub fn len(&self) -> usize {
        match self.is_module.read() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return the transitively flattened directive/pipe class list for `name`.
    ///
    /// If `name` is not a known NgModule, returns `vec![name]` (pass-through —
    /// the caller is treating the identifier as a possible module reference but
    /// it might be a plain directive).
    ///
    /// Cycle-safe: a `visited` set prevents infinite recursion when
    /// pathological NgModules transitively re-export each other.
    /// Order-preserving: depth-first source order with O(n) dedup. The lists
    /// are small (typically <30) so the linear contains-check is fine.
    pub fn flatten(&self, name: &str) -> Vec<String> {
        if let Ok(cache) = self.flat_cache.read() {
            if let Some(hit) = cache.get(name) {
                return hit.clone();
            }
        }
        let mut visited = HashSet::new();
        let mut out = Vec::new();
        self.walk(name, &mut visited, &mut out);
        if let Ok(mut cache) = self.flat_cache.write() {
            cache.insert(name.to_string(), out.clone());
        }
        out
    }

    fn walk(&self, name: &str, visited: &mut HashSet<String>, out: &mut Vec<String>) {
        if !visited.insert(name.to_string()) {
            return;
        }
        let direct = match self.raw_exports.read() {
            Ok(g) => g.get(name).cloned(),
            Err(p) => p.into_inner().get(name).cloned(),
        };
        match direct {
            Some(exports) => {
                for e in exports {
                    if self.is_module(&e) {
                        self.walk(&e, visited, out);
                    } else if !out.iter().any(|x| x == &e) {
                        out.push(e);
                    }
                }
            }
            None => {
                if !out.iter().any(|x| x == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
}

/// Parse an array-literal source text like `"[A, B, C]"` into a list of bare
/// identifier names. Returns `None` if the text is not a simple identifier list
/// (e.g. contains spreads, calls, or non-identifier elements).
///
/// Used by callers that have already extracted the raw source text of an
/// `exports`/`imports`/`declarations` array property via
/// [`crate::metadata::get_source_text`].
pub fn parse_identifier_array(src: &str) -> Option<Vec<String>> {
    let trimmed = src.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    for raw in inner.split(',') {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if !is_identifier(item) {
            return None;
        }
        out.push(item.to_string());
    }
    Some(out)
}

/// Scan every module in `modules` for `ɵɵdefineNgModule(` calls and register
/// the discovered NgModules in `registry`.
///
/// This is Pass A of the standalone-import flattening pipeline. It runs after
/// npm linking (so any `ɵɵngDeclareNgModule` calls have already been rewritten
/// to `ɵɵdefineNgModule` and also registered via the linker's transform hook)
/// and covers two otherwise-missed cases:
///
/// 1. **Project NgModules** — rare in modern Angular apps but legal; emitted
///    by [`ngc_template_compiler::ng_module_codegen`] with `ɵɵdefineNgModule`.
/// 2. **npm packages shipping fully-compiled bundles** that bypass the
///    `ɵɵngDeclare*` partial format.
///
/// Registration is idempotent for identical re-registration, so re-scanning
/// already-registered npm modules is harmless.
///
/// Returns the number of `ɵɵdefineNgModule` calls found.
pub fn scan_define_ng_modules(
    modules: &HashMap<PathBuf, String>,
    registry: &ModuleRegistry,
) -> NgcResult<usize> {
    // `ModuleRegistry` is RwLock-protected, so `scan_one` is thread-safe.
    // Each file's parse + AST walk is independent, so fan out across rayon.
    modules
        .par_iter()
        .filter(|(_, source)| source.contains("\u{0275}\u{0275}defineNgModule"))
        .map(|(path, source)| scan_one(source, path, registry))
        .try_reduce(|| 0, |a, b| Ok(a + b))
}

fn scan_one(source: &str, path: &Path, registry: &ModuleRegistry) -> NgcResult<usize> {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::mjs()).parse();
    if !parsed.errors.is_empty() {
        return Err(NgcError::LinkerError {
            path: path.to_path_buf(),
            message: format!("parse error: {}", parsed.errors[0]),
            line: None,
            column: None,
        });
    }
    let mut count = 0;
    visit_program(&parsed.program, source, registry, &mut count);
    Ok(count)
}

fn visit_program(
    program: &Program<'_>,
    source: &str,
    registry: &ModuleRegistry,
    count: &mut usize,
) {
    for stmt in &program.body {
        visit_stmt(stmt, source, registry, count);
    }
}

fn visit_stmt(stmt: &Statement<'_>, source: &str, registry: &ModuleRegistry, count: &mut usize) {
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            visit_expr(&expr_stmt.expression, source, registry, count);
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    visit_expr(init, source, registry, count);
                }
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(ref d) = export.declaration {
                match d {
                    Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(init) = &declarator.init {
                                visit_expr(init, source, registry, count);
                            }
                        }
                    }
                    Declaration::ClassDeclaration(class) => {
                        visit_class(class, source, registry, count);
                    }
                    _ => {}
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let ExportDefaultDeclarationKind::ClassDeclaration(class) = &export.declaration {
                visit_class(class, source, registry, count);
            }
        }
        Statement::ClassDeclaration(class) => {
            visit_class(class, source, registry, count);
        }
        _ => {}
    }
}

fn visit_class(
    class: &oxc_ast::ast::Class<'_>,
    source: &str,
    registry: &ModuleRegistry,
    count: &mut usize,
) {
    for element in &class.body.body {
        match element {
            oxc_ast::ast::ClassElement::PropertyDefinition(prop) => {
                if let Some(ref init) = prop.value {
                    visit_expr(init, source, registry, count);
                }
            }
            oxc_ast::ast::ClassElement::StaticBlock(block) => {
                for stmt in &block.body {
                    visit_stmt(stmt, source, registry, count);
                }
            }
            _ => {}
        }
    }
}

fn visit_expr(expr: &Expression<'_>, source: &str, registry: &ModuleRegistry, count: &mut usize) {
    match expr {
        Expression::CallExpression(call) if is_define_ng_module(call) => {
            if let Some(obj) = first_object_arg(call) {
                register_from_define(obj, source, registry);
                *count += 1;
            }
        }
        Expression::AssignmentExpression(assign) => {
            visit_expr(&assign.right, source, registry, count);
        }
        Expression::SequenceExpression(seq) => {
            for e in &seq.expressions {
                visit_expr(e, source, registry, count);
            }
        }
        Expression::ClassExpression(class) => {
            visit_class(class, source, registry, count);
        }
        _ => {}
    }
}

fn is_define_ng_module(call: &CallExpression<'_>) -> bool {
    let name = match &call.callee {
        Expression::Identifier(id) => id.name.as_str(),
        Expression::StaticMemberExpression(m) => m.property.name.as_str(),
        _ => return false,
    };
    name.ends_with("defineNgModule")
}

fn first_object_arg<'a>(call: &'a CallExpression<'_>) -> Option<&'a ObjectExpression<'a>> {
    match call.arguments.first()? {
        oxc_ast::ast::Argument::ObjectExpression(obj) => Some(obj.as_ref()),
        _ => None,
    }
}

fn register_from_define(obj: &ObjectExpression<'_>, source: &str, registry: &ModuleRegistry) {
    let name = match metadata::get_identifier_prop(obj, "type") {
        Some(n) => n,
        None => return,
    };
    let exports = metadata::get_source_text(obj, "exports", source)
        .and_then(parse_identifier_array)
        .unwrap_or_default();
    registry.register(&name, exports);
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_passthrough_for_unknown_class() {
        let reg = ModuleRegistry::new();
        assert_eq!(reg.flatten("FormControlName"), vec!["FormControlName"]);
    }

    #[test]
    fn flatten_returns_direct_exports() {
        let reg = ModuleRegistry::new();
        reg.register("MyModule", vec!["DirA".into(), "DirB".into()]);
        assert_eq!(reg.flatten("MyModule"), vec!["DirA", "DirB"]);
    }

    #[test]
    fn flatten_walks_transitive_modules_in_source_order() {
        let reg = ModuleRegistry::new();
        // ReactiveFormsModule shape: re-exports an internal shared module,
        // followed by its own directives.
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
        let flat = reg.flatten("ReactiveFormsModule");
        assert_eq!(
            flat,
            vec![
                "DefaultValueAccessor",
                "NgControlStatus",
                "FormGroupDirective",
                "FormControlName"
            ]
        );
    }

    #[test]
    fn flatten_dedups_duplicate_directives_across_modules() {
        let reg = ModuleRegistry::new();
        reg.register("ModA", vec!["SharedDir".into(), "DirA".into()]);
        reg.register("ModB", vec!["SharedDir".into(), "DirB".into()]);
        reg.register("Combined", vec!["ModA".into(), "ModB".into()]);
        let flat = reg.flatten("Combined");
        assert_eq!(flat, vec!["SharedDir", "DirA", "DirB"]);
    }

    #[test]
    fn flatten_handles_cycles() {
        let reg = ModuleRegistry::new();
        reg.register("A", vec!["B".into(), "DirA".into()]);
        reg.register("B", vec!["A".into(), "DirB".into()]);
        let flat = reg.flatten("A");
        // Walks A -> B (skips A re-entry) -> DirB, then DirA.
        assert_eq!(flat, vec!["DirB", "DirA"]);
    }

    #[test]
    fn flatten_caches_result() {
        let reg = ModuleRegistry::new();
        reg.register("M", vec!["D".into()]);
        let first = reg.flatten("M");
        let second = reg.flatten("M");
        assert_eq!(first, second);
    }

    #[test]
    fn register_invalidates_flat_cache() {
        let reg = ModuleRegistry::new();
        reg.register("M", vec!["DirOld".into()]);
        let _ = reg.flatten("M");
        reg.register("M", vec!["DirNew".into()]);
        assert_eq!(reg.flatten("M"), vec!["DirNew"]);
    }

    #[test]
    fn is_module_reflects_registration() {
        let reg = ModuleRegistry::new();
        assert!(!reg.is_module("M"));
        reg.register("M", vec![]);
        assert!(reg.is_module("M"));
    }

    #[test]
    fn parse_identifier_array_basic() {
        assert_eq!(
            parse_identifier_array("[A, B, C]"),
            Some(vec!["A".into(), "B".into(), "C".into()])
        );
    }

    #[test]
    fn parse_identifier_array_handles_whitespace() {
        assert_eq!(
            parse_identifier_array("[\n  A,\n  B,\n]"),
            Some(vec!["A".into(), "B".into()])
        );
    }

    #[test]
    fn parse_identifier_array_rejects_non_identifier_elements() {
        assert!(parse_identifier_array("[a.b, c]").is_none());
        assert!(parse_identifier_array("[fn(), c]").is_none());
        assert!(parse_identifier_array("[...spread, c]").is_none());
    }

    #[test]
    fn parse_identifier_array_accepts_empty() {
        assert_eq!(parse_identifier_array("[]"), Some(vec![]));
    }

    #[test]
    fn parse_identifier_array_rejects_non_array() {
        assert!(parse_identifier_array("foo").is_none());
        assert!(parse_identifier_array("(a, b)").is_none());
    }

    #[test]
    fn register_is_idempotent_for_identical_exports() {
        let reg = ModuleRegistry::new();
        reg.register("M", vec!["A".into(), "B".into()]);
        reg.register("M", vec!["A".into(), "B".into()]);
        assert_eq!(reg.flatten("M"), vec!["A", "B"]);
    }

    #[test]
    fn scan_registers_project_define_ng_module() {
        let mut modules = HashMap::new();
        let source = "class AppModule {}\n\
AppModule.\u{0275}fac = function AppModule_Factory(t) { return new (t || AppModule)(); };\n\
AppModule.\u{0275}mod = \u{0275}\u{0275}defineNgModule({ type: AppModule, declarations: [MyComp], imports: [SubModule], exports: [MyComp] });\n\
AppModule.\u{0275}inj = \u{0275}\u{0275}defineInjector({});\n\
export { AppModule };";
        modules.insert(
            PathBuf::from("/project/src/app.module.js"),
            source.to_string(),
        );

        let registry = ModuleRegistry::new();
        let count = scan_define_ng_modules(&modules, &registry).unwrap();
        assert_eq!(count, 1);
        assert!(registry.is_module("AppModule"));
        assert_eq!(registry.flatten("AppModule"), vec!["MyComp"]);
    }

    #[test]
    fn scan_is_idempotent_over_already_registered_npm_module() {
        // Mimics the case where the npm link pass has already rewritten
        // ɵɵngDeclareNgModule to ɵɵdefineNgModule and registered the module.
        // Pass A then re-visits the same file and registration should be a no-op.
        let registry = ModuleRegistry::new();
        registry.register(
            "ReactiveFormsModule",
            vec!["FormGroupDirective".into(), "FormControlName".into()],
        );

        let mut modules = HashMap::new();
        let source = "i0.\u{0275}\u{0275}defineNgModule({ type: ReactiveFormsModule, exports: [FormGroupDirective, FormControlName] });";
        modules.insert(
            PathBuf::from("/node_modules/@angular/forms/index.mjs"),
            source.to_string(),
        );

        let count = scan_define_ng_modules(&modules, &registry).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            registry.flatten("ReactiveFormsModule"),
            vec!["FormGroupDirective", "FormControlName"]
        );
    }

    #[test]
    fn scan_skips_files_without_define_ng_module() {
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/project/src/utils.js"),
            "export function add(a, b) { return a + b; }".to_string(),
        );
        let registry = ModuleRegistry::new();
        let count = scan_define_ng_modules(&modules, &registry).unwrap();
        assert_eq!(count, 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn scan_handles_define_ng_module_without_exports() {
        let mut modules = HashMap::new();
        let source = "class EmptyModule {}\n\
EmptyModule.\u{0275}mod = \u{0275}\u{0275}defineNgModule({ type: EmptyModule });\n\
export { EmptyModule };";
        modules.insert(
            PathBuf::from("/project/src/empty.module.js"),
            source.to_string(),
        );

        let registry = ModuleRegistry::new();
        let count = scan_define_ng_modules(&modules, &registry).unwrap();
        assert_eq!(count, 1);
        assert!(registry.is_module("EmptyModule"));
        assert!(registry.flatten("EmptyModule").is_empty());
    }
}
