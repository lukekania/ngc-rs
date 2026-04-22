use std::collections::{BTreeSet, HashMap, HashSet};

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{ExportDefaultDeclarationKind, Expression, ModuleDeclaration, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// A module after import/export rewriting for bundling.
#[derive(Debug, Clone)]
pub struct RewrittenModule {
    /// The module code with local imports and exports stripped.
    pub code: String,
    /// External imports collected from this module.
    pub external_imports: Vec<ExternalImport>,
    /// Dynamic imports found in this module.
    pub dynamic_imports: Vec<DynamicImportInfo>,
}

/// An external import that should be hoisted to the top of the bundle.
#[derive(Debug, Clone)]
pub struct ExternalImport {
    /// The import source (e.g. `@angular/core`).
    pub source: String,
    /// The default import binding, if any (e.g. `_decorate`).
    pub default_import: Option<String>,
    /// Named import bindings (e.g. `Component`, `RouterOutlet`).
    pub named_imports: BTreeSet<String>,
    /// Whether this is a side-effect-only import (`import 'zone.js'`).
    pub is_side_effect: bool,
}

/// Information about a dynamic `import()` expression found in a module.
#[derive(Debug, Clone)]
pub struct DynamicImportInfo {
    /// The original import specifier string.
    pub original_specifier: String,
}

/// A text edit to apply to the source: either a removal or a replacement.
struct TextEdit {
    start: u32,
    end: u32,
    /// `None` means removal, `Some(text)` means replacement.
    replacement: Option<String>,
}

/// Rewrite a single JavaScript module for bundling.
///
/// Parses the JS code, classifies each import as local or external based on
/// `local_prefixes`, strips local imports and export keywords, collects
/// external imports for hoisting, and rewrites dynamic `import()` specifiers
/// according to `dynamic_import_rewrites`.
#[cfg(test)]
pub fn rewrite_module(
    js_code: &str,
    file_name: &str,
    local_prefixes: &[&str],
    dynamic_import_rewrites: &HashMap<String, String>,
) -> NgcResult<RewrittenModule> {
    rewrite_module_with_shaking(
        js_code,
        file_name,
        local_prefixes,
        dynamic_import_rewrites,
        None,
        &HashSet::new(),
        &HashMap::new(),
        false,
    )
}

/// Rewrite a module with optional tree shaking of unused exports.
///
/// When `unused_exports` is provided, declarations of exports in that set
/// are fully removed (not just the `export` keyword stripped).
#[allow(clippy::too_many_arguments)]
pub fn rewrite_module_with_shaking(
    js_code: &str,
    file_name: &str,
    local_prefixes: &[&str],
    dynamic_import_rewrites: &HashMap<String, String>,
    unused_exports: Option<&HashSet<String>>,
    bundled_specifiers: &HashSet<String>,
    namespace_map: &HashMap<String, String>,
    preserve_exports: bool,
) -> NgcResult<RewrittenModule> {
    let allocator = Allocator::new();
    // Use tsx() which is a superset of mjs — handles both JS and TS sources.
    // Files that failed the TS→JS transform step may still have TS annotations.
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, js_code, source_type).parse();

    if parsed.panicked {
        return Err(NgcError::BundleError {
            message: format!("failed to parse {file_name} for bundling"),
        });
    }

    let mut edits: Vec<TextEdit> = Vec::new();
    let mut external_imports: Vec<ExternalImport> = Vec::new();
    let mut dynamic_imports: Vec<DynamicImportInfo> = Vec::new();

    for stmt in &parsed.program.body {
        // Walk top-level module declarations (import/export).
        // `fully_removed` is true when the statement's span was elided or
        // entirely replaced, so nested dynamic imports are dead code and
        // must not be rewritten — their length-changing edits would shift
        // byte offsets and break the removal edit.
        let fully_removed = if let Some(module_decl) = stmt.as_module_declaration() {
            collect_module_decl_edits(
                module_decl,
                local_prefixes,
                &mut edits,
                &mut external_imports,
                unused_exports,
                bundled_specifiers,
                namespace_map,
                preserve_exports,
            )
        } else {
            false
        };

        if !fully_removed {
            collect_dynamic_import_edits(
                stmt,
                dynamic_import_rewrites,
                &mut edits,
                &mut dynamic_imports,
            );
        }
    }

    let code = apply_edits(js_code, &mut edits);

    Ok(RewrittenModule {
        code,
        external_imports,
        dynamic_imports,
    })
}

/// Process a top-level module declaration (import/export) and collect edits.
///
/// Returns `true` when the statement's full span was elided from output
/// (either removed outright or replaced wholesale). The caller uses this
/// signal to skip nested dynamic-import rewriting for that statement —
/// nested `import()` calls inside a fully-removed span are dead code, and
/// their length-changing edits would shift byte offsets and corrupt the
/// removal edit's `end` position.
#[allow(clippy::too_many_arguments)]
fn collect_module_decl_edits(
    module_decl: &ModuleDeclaration,
    local_prefixes: &[&str],
    edits: &mut Vec<TextEdit>,
    external_imports: &mut Vec<ExternalImport>,
    unused_exports: Option<&HashSet<String>>,
    bundled_specifiers: &HashSet<String>,
    namespace_map: &HashMap<String, String>,
    preserve_exports: bool,
) -> bool {
    match module_decl {
        ModuleDeclaration::ImportDeclaration(import) => {
            let source = import.source.value.as_str();
            if is_local(source, local_prefixes, bundled_specifiers) {
                // Check if this import has a namespace mapping (npm module)
                if let Some(ns) = namespace_map.get(source) {
                    // Replace import with namespace lookups
                    let mut replacements = Vec::new();
                    if let Some(specifiers) = &import.specifiers {
                        for spec in specifiers {
                            match spec {
                                oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                    let imported = s.imported.name();
                                    let local = &s.local.name;
                                    replacements
                                        .push(format!("var {local} = {ns}.{imported};"));
                                }
                                oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(
                                    s,
                                ) => {
                                    let local = &s.local.name;
                                    replacements
                                        .push(format!("var {local} = {ns}.default;"));
                                }
                                oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(
                                    s,
                                ) => {
                                    let local = &s.local.name;
                                    replacements.push(format!("var {local} = {ns};"));
                                }
                            }
                        }
                    }
                    let replacement = if replacements.is_empty() {
                        None
                    } else {
                        Some(replacements.join("\n"))
                    };
                    edits.push(TextEdit {
                        start: import.span.start,
                        end: import.span.end,
                        replacement,
                    });
                } else {
                    // Regular local import — strip entirely
                    edits.push(TextEdit {
                        start: import.span.start,
                        end: import.span.end,
                        replacement: None,
                    });
                }
            } else {
                let mut named = BTreeSet::new();
                let mut default = None;
                let mut is_side_effect = true;

                if let Some(specifiers) = &import.specifiers {
                    for spec in specifiers {
                        is_side_effect = false;
                        match spec {
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                named.insert(s.local.name.to_string());
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                default = Some(s.local.name.to_string());
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(
                                s,
                            ) => {
                                named.insert(format!("* as {}", s.local.name));
                            }
                        }
                    }
                }

                external_imports.push(ExternalImport {
                    source: source.to_string(),
                    default_import: default,
                    named_imports: named,
                    is_side_effect,
                });
                edits.push(TextEdit {
                    start: import.span.start,
                    end: import.span.end,
                    replacement: None,
                });
            }
            true
        }
        ModuleDeclaration::ExportNamedDeclaration(export) => {
            if preserve_exports {
                // Keep exports intact for lazy chunk entry modules
                false
            } else if export.source.is_some() {
                edits.push(TextEdit {
                    start: export.span.start,
                    end: export.span.end,
                    replacement: None,
                });
                true
            } else if let Some(decl) = &export.declaration {
                // Check if this export's declaration name is in the unused set
                let decl_name = get_declaration_name(decl);
                let is_unused = decl_name
                    .as_ref()
                    .is_some_and(|name| unused_exports.is_some_and(|unused| unused.contains(name)));

                if is_unused {
                    // Remove the entire declaration, not just the export keyword
                    edits.push(TextEdit {
                        start: export.span.start,
                        end: export.span.end,
                        replacement: None,
                    });
                    true
                } else {
                    // Just strip the "export " keyword; body is preserved
                    // so nested dynamic imports still need rewriting.
                    edits.push(TextEdit {
                        start: export.span.start,
                        end: export.span.start + 7, // "export "
                        replacement: None,
                    });
                    false
                }
            } else {
                edits.push(TextEdit {
                    start: export.span.start,
                    end: export.span.end,
                    replacement: None,
                });
                true
            }
        }
        ModuleDeclaration::ExportDefaultDeclaration(export) => {
            if preserve_exports {
                // Keep exports intact for lazy chunk entry modules
                false
            } else {
                match &export.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(_)
                    | ExportDefaultDeclarationKind::ClassDeclaration(_) => {
                        // Only strip "export default "; body preserved.
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.start + 15, // "export default "
                            replacement: None,
                        });
                        false
                    }
                    _ => {
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.end,
                            replacement: None,
                        });
                        true
                    }
                }
            }
        }
        ModuleDeclaration::ExportAllDeclaration(export)
            if is_local(
                export.source.value.as_str(),
                local_prefixes,
                bundled_specifiers,
            ) =>
        {
            edits.push(TextEdit {
                start: export.span.start,
                end: export.span.end,
                replacement: None,
            });
            true
        }
        _ => false,
    }
}

/// Walk a statement recursively to find dynamic `import()` expressions.
fn collect_dynamic_import_edits(
    stmt: &Statement,
    rewrites: &HashMap<String, String>,
    edits: &mut Vec<TextEdit>,
    dynamic_imports: &mut Vec<DynamicImportInfo>,
) {
    // Walk expressions within this statement
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            walk_expr_for_dynamic_imports(&expr_stmt.expression, rewrites, edits, dynamic_imports);
        }
        Statement::VariableDeclaration(decl) => {
            for declarator in &decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expr_for_dynamic_imports(init, rewrites, edits, dynamic_imports);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                walk_expr_for_dynamic_imports(arg, rewrites, edits, dynamic_imports);
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                collect_dynamic_import_edits_from_decl(decl, rewrites, edits, dynamic_imports);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &export.declaration {
                if let Some(body) = &f.body {
                    for s in &body.statements {
                        collect_dynamic_import_edits(s, rewrites, edits, dynamic_imports);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Walk a declaration for dynamic imports (used inside export statements).
fn collect_dynamic_import_edits_from_decl(
    decl: &oxc_ast::ast::Declaration,
    rewrites: &HashMap<String, String>,
    edits: &mut Vec<TextEdit>,
    dynamic_imports: &mut Vec<DynamicImportInfo>,
) {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    walk_expr_for_dynamic_imports(init, rewrites, edits, dynamic_imports);
                }
            }
        }
        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for s in &body.statements {
                    collect_dynamic_import_edits(s, rewrites, edits, dynamic_imports);
                }
            }
        }
        _ => {}
    }
}

/// Recursively walk an expression tree to find `import()` calls and worker
/// URL constructions (`new Worker(new URL(...))` / `new SharedWorker(...)`).
fn walk_expr_for_dynamic_imports(
    expr: &Expression,
    rewrites: &HashMap<String, String>,
    edits: &mut Vec<TextEdit>,
    dynamic_imports: &mut Vec<DynamicImportInfo>,
) {
    match expr {
        Expression::ImportExpression(import_expr) => {
            // Extract the specifier from the source argument
            if let Expression::StringLiteral(lit) = &import_expr.source {
                let specifier = lit.value.as_str();
                dynamic_imports.push(DynamicImportInfo {
                    original_specifier: specifier.to_string(),
                });

                // Rewrite if we have a mapping
                if let Some(chunk_filename) = rewrites.get(specifier) {
                    // Replace the entire string literal (including quotes) with new value
                    edits.push(TextEdit {
                        start: lit.span.start,
                        end: lit.span.end,
                        replacement: Some(format!("'./{chunk_filename}'")),
                    });
                }
            }
        }
        Expression::NewExpression(new_expr) => {
            // Detect `new Worker(new URL('<spec>', import.meta.url), ...)`
            // and `new SharedWorker(...)`. Rewrite the inner URL string
            // literal to point to the emitted worker chunk filename.
            if is_worker_callee(&new_expr.callee) {
                if let Some(Expression::NewExpression(url_expr)) =
                    new_expr.arguments.first().and_then(|a| a.as_expression())
                {
                    if is_url_callee(&url_expr.callee)
                        && url_expr
                            .arguments
                            .get(1)
                            .and_then(|a| a.as_expression())
                            .is_some_and(is_import_meta_url)
                    {
                        if let Some(Expression::StringLiteral(lit)) =
                            url_expr.arguments.first().and_then(|a| a.as_expression())
                        {
                            let specifier = lit.value.as_str();
                            if let Some(chunk_filename) = rewrites.get(specifier) {
                                edits.push(TextEdit {
                                    start: lit.span.start,
                                    end: lit.span.end,
                                    replacement: Some(format!("'./{chunk_filename}'")),
                                });
                            }
                        }
                    }
                }
            }
            // Still recurse into the callee + arguments so nested dynamic
            // imports or worker URLs inside the arguments also get visited.
            walk_expr_for_dynamic_imports(&new_expr.callee, rewrites, edits, dynamic_imports);
            for arg in &new_expr.arguments {
                if let Some(expr) = arg.as_expression() {
                    walk_expr_for_dynamic_imports(expr, rewrites, edits, dynamic_imports);
                }
            }
        }
        Expression::CallExpression(call) => {
            walk_expr_for_dynamic_imports(&call.callee, rewrites, edits, dynamic_imports);
            for arg in &call.arguments {
                if let Some(expr) = arg.as_expression() {
                    walk_expr_for_dynamic_imports(expr, rewrites, edits, dynamic_imports);
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            // Walk the body — could be an expression or statements
            if arrow.expression {
                // Single expression body
                if let Some(Statement::ExpressionStatement(expr_stmt)) =
                    arrow.body.statements.first()
                {
                    walk_expr_for_dynamic_imports(
                        &expr_stmt.expression,
                        rewrites,
                        edits,
                        dynamic_imports,
                    );
                }
            } else {
                for s in &arrow.body.statements {
                    collect_dynamic_import_edits(s, rewrites, edits, dynamic_imports);
                }
            }
        }
        // Handle member expressions (StaticMember, ComputedMember, PrivateField)
        _ if expr.as_member_expression().is_some() => {
            let member = expr.as_member_expression().expect("checked above");
            walk_expr_for_dynamic_imports(member.object(), rewrites, edits, dynamic_imports);
        }
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let oxc_ast::ast::ArrayExpressionElement::SpreadElement(spread) = elem {
                    walk_expr_for_dynamic_imports(
                        &spread.argument,
                        rewrites,
                        edits,
                        dynamic_imports,
                    );
                } else if let Some(expr) = elem.as_expression() {
                    walk_expr_for_dynamic_imports(expr, rewrites, edits, dynamic_imports);
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                match prop {
                    oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) => {
                        walk_expr_for_dynamic_imports(&p.value, rewrites, edits, dynamic_imports);
                    }
                    oxc_ast::ast::ObjectPropertyKind::SpreadProperty(spread) => {
                        walk_expr_for_dynamic_imports(
                            &spread.argument,
                            rewrites,
                            edits,
                            dynamic_imports,
                        );
                    }
                }
            }
        }
        Expression::ConditionalExpression(cond) => {
            walk_expr_for_dynamic_imports(&cond.test, rewrites, edits, dynamic_imports);
            walk_expr_for_dynamic_imports(&cond.consequent, rewrites, edits, dynamic_imports);
            walk_expr_for_dynamic_imports(&cond.alternate, rewrites, edits, dynamic_imports);
        }
        Expression::LogicalExpression(logic) => {
            walk_expr_for_dynamic_imports(&logic.left, rewrites, edits, dynamic_imports);
            walk_expr_for_dynamic_imports(&logic.right, rewrites, edits, dynamic_imports);
        }
        Expression::AssignmentExpression(assign) => {
            walk_expr_for_dynamic_imports(&assign.right, rewrites, edits, dynamic_imports);
        }
        Expression::SequenceExpression(seq) => {
            for expr in &seq.expressions {
                walk_expr_for_dynamic_imports(expr, rewrites, edits, dynamic_imports);
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            walk_expr_for_dynamic_imports(&paren.expression, rewrites, edits, dynamic_imports);
        }
        Expression::AwaitExpression(aw) => {
            walk_expr_for_dynamic_imports(&aw.argument, rewrites, edits, dynamic_imports);
        }
        Expression::TemplateLiteral(tl) => {
            for expr in &tl.expressions {
                walk_expr_for_dynamic_imports(expr, rewrites, edits, dynamic_imports);
            }
        }
        // For other expression types, we don't recurse (no nested import() possible)
        _ => {}
    }
}

/// True when `expr` is a plain identifier reference to `Worker` or `SharedWorker`.
fn is_worker_callee(expr: &Expression) -> bool {
    if let Expression::Identifier(id) = expr {
        matches!(id.name.as_str(), "Worker" | "SharedWorker")
    } else {
        false
    }
}

/// True when `expr` is a plain identifier reference to `URL`.
fn is_url_callee(expr: &Expression) -> bool {
    matches!(expr, Expression::Identifier(id) if id.name.as_str() == "URL")
}

/// True when `expr` is the `import.meta.url` member expression.
fn is_import_meta_url(expr: &Expression) -> bool {
    let member = match expr.as_member_expression() {
        Some(m) => m,
        None => return false,
    };
    // Property must be `url`
    let Some(prop_name) = member.static_property_name() else {
        return false;
    };
    if prop_name != "url" {
        return false;
    }
    // Object must be a MetaProperty whose meta is `import` and property is `meta`
    matches!(
        member.object(),
        Expression::MetaProperty(mp)
            if mp.meta.name.as_str() == "import" && mp.property.name.as_str() == "meta"
    )
}

/// Extract the declared name from a declaration, if it has a single clear name.
fn get_declaration_name(decl: &oxc_ast::ast::Declaration) -> Option<String> {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(var) => {
            if let Some(declarator) = var.declarations.first() {
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    return Some(id.name.to_string());
                }
            }
            None
        }
        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
            f.id.as_ref().map(|id| id.name.to_string())
        }
        oxc_ast::ast::Declaration::ClassDeclaration(c) => {
            c.id.as_ref().map(|id| id.name.to_string())
        }
        _ => None,
    }
}

/// Check if an import specifier is local based on known prefixes or bundled specifiers.
fn is_local(
    specifier: &str,
    local_prefixes: &[&str],
    bundled_specifiers: &HashSet<String>,
) -> bool {
    local_prefixes
        .iter()
        .any(|prefix| specifier.starts_with(prefix))
        || bundled_specifiers.contains(specifier)
}

/// Apply text edits to the source, producing the rewritten code.
fn apply_edits(source: &str, edits: &mut [TextEdit]) -> String {
    // Sort in reverse order so later edits don't shift earlier offsets
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));

    let mut result = source.to_string();
    for edit in edits.iter() {
        let start = edit.start as usize;
        let end = edit.end as usize;
        if start <= result.len() && end <= result.len() {
            match &edit.replacement {
                Some(new_text) => {
                    result.replace_range(start..end, new_text);
                }
                None => {
                    // Removal: also remove trailing newline if present
                    let actual_end = if end < result.len() && result.as_bytes()[end] == b'\n' {
                        end + 1
                    } else {
                        end
                    };
                    result.replace_range(start..actual_end, "");
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_rewrites() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn test_external_named_import_collected_and_removed() {
        let code = "import { Component } from '@angular/core';\nclass Foo {}\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert!(!result.code.contains("import"));
        assert_eq!(result.external_imports.len(), 1);
        assert_eq!(result.external_imports[0].source, "@angular/core");
        assert!(result.external_imports[0]
            .named_imports
            .contains("Component"));
    }

    #[test]
    fn test_external_default_import_collected() {
        let code = "import _decorate from '@oxc-project/runtime/helpers/decorate';\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert_eq!(
            result.external_imports[0].default_import,
            Some("_decorate".to_string())
        );
    }

    #[test]
    fn test_local_relative_import_removed() {
        let code = "import { Foo } from './foo';\nconst x = 1;\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert!(!result.code.contains("import"));
        assert!(result.code.contains("const x = 1"));
        assert!(result.external_imports.is_empty());
    }

    #[test]
    fn test_local_alias_import_removed() {
        let code = "import { SharedUtils } from '@app/shared';\n";
        let result = rewrite_module(code, "test.js", &[".", "@app/"], &empty_rewrites())
            .expect("should rewrite");
        assert!(!result.code.contains("import"));
        assert!(result.external_imports.is_empty());
    }

    #[test]
    fn test_reexport_removed() {
        let code = "export { SharedUtils } from './utils';\nexport { Logger } from './logger';\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert!(!result.code.contains("export"));
        assert!(!result.code.contains("SharedUtils"));
    }

    #[test]
    fn test_export_class_keyword_stripped() {
        let code = "export class Logger {\n\tstatic log(msg) {}\n}\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert!(result.code.contains("class Logger"));
        assert!(!result.code.contains("export"));
    }

    #[test]
    fn test_export_const_keyword_stripped() {
        let code = "export const routes = [];\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert!(result.code.contains("const routes = []"));
        assert!(!result.code.contains("export"));
    }

    #[test]
    fn test_export_list_removed() {
        let code = "let AppComponent = class AppComponent {};\nexport { AppComponent };\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert!(result.code.contains("let AppComponent"));
        assert!(!result.code.contains("export"));
    }

    #[test]
    fn test_side_effect_external_import() {
        let code = "import 'zone.js';\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert_eq!(result.external_imports.len(), 1);
        assert!(result.external_imports[0].is_side_effect);
        assert_eq!(result.external_imports[0].source, "zone.js");
    }

    #[test]
    fn test_module_with_no_imports() {
        let code = "const x = 42;\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert_eq!(result.code.trim(), "const x = 42;");
        assert!(result.external_imports.is_empty());
    }

    #[test]
    fn test_dynamic_import_detected() {
        let code = "const m = import('./lazy-module');\n";
        let result =
            rewrite_module(code, "test.js", &["."], &empty_rewrites()).expect("should rewrite");
        assert_eq!(result.dynamic_imports.len(), 1);
        assert_eq!(
            result.dynamic_imports[0].original_specifier,
            "./lazy-module"
        );
        // Without rewrites, the import() should pass through as-is
        assert!(result.code.contains("import('./lazy-module')"));
    }

    #[test]
    fn test_dynamic_import_rewritten() {
        let code = "const m = import('./admin/admin.component');\n";
        let mut rewrites = HashMap::new();
        rewrites.insert(
            "./admin/admin.component".to_string(),
            "chunk-admin-component.js".to_string(),
        );
        let result = rewrite_module(code, "test.js", &["."], &rewrites).expect("should rewrite");
        assert!(result.code.contains("'./chunk-admin-component.js'"));
        assert!(!result.code.contains("./admin/admin.component"));
    }

    #[test]
    fn test_dynamic_import_in_arrow_function() {
        let code = "const routes = [{ path: 'admin', loadComponent: () => import('./admin').then(m => m.Admin) }];\n";
        let mut rewrites = HashMap::new();
        rewrites.insert("./admin".to_string(), "chunk-admin.js".to_string());
        let result = rewrite_module(code, "test.js", &["."], &rewrites).expect("should rewrite");
        assert!(result.code.contains("'./chunk-admin.js'"));
        assert!(!result.code.contains("import('./admin')"));
        assert_eq!(result.dynamic_imports.len(), 1);
    }

    #[test]
    fn test_dynamic_import_in_export_const() {
        let code =
            "export const routes = [{ loadComponent: () => import('./lazy').then(m => m.C) }];\n";
        let mut rewrites = HashMap::new();
        rewrites.insert("./lazy".to_string(), "chunk-lazy.js".to_string());
        let result = rewrite_module(code, "test.js", &["."], &rewrites).expect("should rewrite");
        assert!(result.code.contains("'./chunk-lazy.js'"));
        assert_eq!(result.dynamic_imports.len(), 1);
    }

    #[test]
    fn test_worker_url_specifier_rewritten() {
        let code =
            "const w = new Worker(new URL('./compute.worker', import.meta.url), { type: 'module' });\n";
        let mut rewrites = HashMap::new();
        rewrites.insert(
            "./compute.worker".to_string(),
            "worker-compute.js".to_string(),
        );
        let result = rewrite_module(code, "test.js", &["."], &rewrites).expect("should rewrite");
        assert!(result.code.contains("'./worker-compute.js'"));
        assert!(!result.code.contains("./compute.worker"));
        assert!(result.code.contains("new Worker(new URL("));
        assert!(result.code.contains("import.meta.url"));
    }

    #[test]
    fn test_shared_worker_url_specifier_rewritten() {
        let code = r#"const w = new SharedWorker(new URL("./shared.worker", import.meta.url));"#;
        let mut rewrites = HashMap::new();
        rewrites.insert(
            "./shared.worker".to_string(),
            "worker-shared.js".to_string(),
        );
        let result = rewrite_module(code, "test.js", &["."], &rewrites).expect("should rewrite");
        assert!(result.code.contains("'./worker-shared.js'"));
        assert!(!result.code.contains("./shared.worker"));
    }

    #[test]
    fn test_worker_without_import_meta_url_not_rewritten() {
        // A `new Worker(new URL(...))` that uses something other than
        // `import.meta.url` as the base isn't a bundled worker entrypoint;
        // leave the specifier alone.
        let code = "const w = new Worker(new URL('./compute.worker', location.href));\n";
        let mut rewrites = HashMap::new();
        rewrites.insert(
            "./compute.worker".to_string(),
            "worker-compute.js".to_string(),
        );
        let result = rewrite_module(code, "test.js", &["."], &rewrites).expect("should rewrite");
        assert!(!result.code.contains("worker-compute.js"));
        assert!(result.code.contains("./compute.worker"));
    }

    #[test]
    fn test_unused_export_const_with_nested_dynamic_import_fully_removed() {
        // Issue #13 repro: a tree-shaken export containing a dynamic import
        // must not produce both a removal edit and a lengthening specifier
        // rewrite — the removal's `end` offset would point inside the grown
        // replacement, leaving a stray `export` keyword.
        let code =
            "export const routes = [{ loadComponent: () => import('./lazy').then(m => m.C) }];\n";
        let mut rewrites = HashMap::new();
        rewrites.insert("./lazy".to_string(), "chunk-lazy.js".to_string());
        let mut unused = HashSet::new();
        unused.insert("routes".to_string());

        let result = rewrite_module_with_shaking(
            code,
            "test.js",
            &["."],
            &rewrites,
            Some(&unused),
            &HashSet::new(),
            &HashMap::new(),
            false,
        )
        .expect("should rewrite");

        assert!(
            result.code.trim().is_empty(),
            "expected empty output, got: {:?}",
            result.code
        );
        assert!(!result.code.contains("export"));
        assert!(!result.code.contains("chunk-lazy.js"));
        assert!(!result.code.contains("./lazy"));
        assert!(result.dynamic_imports.is_empty());
    }

    #[test]
    fn test_used_export_const_with_nested_dynamic_import_is_rewritten() {
        // Opposite of the prior test: when the export is used, the body is
        // kept (only `export ` stripped) and the nested dynamic import is
        // rewritten to its chunk filename.
        let code =
            "export const routes = [{ loadComponent: () => import('./lazy').then(m => m.C) }];\n";
        let mut rewrites = HashMap::new();
        rewrites.insert("./lazy".to_string(), "chunk-lazy.js".to_string());
        let empty_unused: HashSet<String> = HashSet::new();

        let result = rewrite_module_with_shaking(
            code,
            "test.js",
            &["."],
            &rewrites,
            Some(&empty_unused),
            &HashSet::new(),
            &HashMap::new(),
            false,
        )
        .expect("should rewrite");

        assert!(result.code.contains("const routes"));
        assert!(!result.code.contains("export"));
        assert!(result.code.contains("'./chunk-lazy.js'"));
        assert!(!result.code.contains("import('./lazy')"));
        assert_eq!(result.dynamic_imports.len(), 1);
    }

    #[test]
    fn test_unused_export_function_with_nested_dynamic_import_fully_removed() {
        let code = "export function loadIt() { return import('./lazy'); }\n";
        let mut rewrites = HashMap::new();
        rewrites.insert("./lazy".to_string(), "chunk-lazy.js".to_string());
        let mut unused = HashSet::new();
        unused.insert("loadIt".to_string());

        let result = rewrite_module_with_shaking(
            code,
            "test.js",
            &["."],
            &rewrites,
            Some(&unused),
            &HashSet::new(),
            &HashMap::new(),
            false,
        )
        .expect("should rewrite");

        assert!(
            result.code.trim().is_empty(),
            "expected empty output, got: {:?}",
            result.code
        );
        assert!(!result.code.contains("export"));
        assert!(!result.code.contains("chunk-lazy.js"));
        assert!(!result.code.contains("./lazy"));
        assert!(result.dynamic_imports.is_empty());
    }

    #[test]
    fn test_used_export_function_with_nested_dynamic_import_is_rewritten() {
        let code = "export function loadIt() { return import('./lazy'); }\n";
        let mut rewrites = HashMap::new();
        rewrites.insert("./lazy".to_string(), "chunk-lazy.js".to_string());
        let empty_unused: HashSet<String> = HashSet::new();

        let result = rewrite_module_with_shaking(
            code,
            "test.js",
            &["."],
            &rewrites,
            Some(&empty_unused),
            &HashSet::new(),
            &HashMap::new(),
            false,
        )
        .expect("should rewrite");

        assert!(result.code.contains("function loadIt"));
        assert!(!result.code.contains("export"));
        assert!(result.code.contains("'./chunk-lazy.js'"));
        assert!(!result.code.contains("import('./lazy')"));
        assert_eq!(result.dynamic_imports.len(), 1);
    }
}
