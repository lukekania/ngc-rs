//! IIFE wrapping for npm modules.
//!
//! Wraps each npm module in an immediately-invoked function expression (IIFE)
//! with a unique namespace variable. This isolates top-level declarations
//! to prevent name collisions between different npm packages.
//!
//! ## Output pattern
//!
//! ```js
//! var __ns_abc123 = {};
//! (function(__exports) {
//!   var Component = __ns_def456.Component;  // rewritten imports
//!   function MyClass() { ... }
//!   __exports.MyClass = MyClass;            // export assignments
//! })(__ns_abc123);
//! ```

use std::collections::BTreeSet;
use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{ExportDefaultDeclarationKind, ImportDeclarationSpecifier, ModuleDeclaration};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Information needed to wrap a single npm module.
pub struct NpmModuleInfo {
    /// The IIFE-wrapped code for this module.
    pub wrapped_code: String,
    /// Names exported by this module.
    pub exported_names: Vec<String>,
}

/// Wrap a single npm module in an IIFE with namespace isolation.
///
/// Rewrites imports to namespace lookups, strips export keywords, wraps the
/// code in an IIFE, and adds export assignments.
///
/// `resolve_import` is a closure that maps an import specifier to a namespace
/// variable name, or `None` if the import should be left as-is (truly external).
pub fn wrap_npm_module<F>(
    js_code: &str,
    file_name: &str,
    namespace: &str,
    resolve_import: F,
) -> NgcResult<NpmModuleInfo>
where
    F: Fn(&str) -> Option<String>,
{
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, js_code, SourceType::mjs()).parse();

    if parsed.panicked {
        return Err(NgcError::BundleError {
            message: format!("npm wrap: failed to parse {file_name}"),
        });
    }

    let mut edits: Vec<TextEdit> = Vec::new();
    let mut exported_names: Vec<String> = Vec::new();
    let mut has_default_export = false;

    for stmt in &parsed.program.body {
        if let Some(module_decl) = stmt.as_module_declaration() {
            match module_decl {
                ModuleDeclaration::ImportDeclaration(import) => {
                    let source = import.source.value.as_str();
                    if let Some(target_ns) = resolve_import(source) {
                        // Rewrite import to namespace lookups
                        let mut replacements = Vec::new();
                        if let Some(specifiers) = &import.specifiers {
                            for spec in specifiers {
                                match spec {
                                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                        let imported = s.imported.name();
                                        let local = &s.local.name;
                                        replacements
                                            .push(format!("var {local} = {target_ns}.{imported};"));
                                    }
                                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                        let local = &s.local.name;
                                        replacements
                                            .push(format!("var {local} = {target_ns}.default;"));
                                    }
                                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                                        let local = &s.local.name;
                                        replacements.push(format!("var {local} = {target_ns};"));
                                    }
                                }
                            }
                        }
                        // Side-effect import: just remove (the target IIFE already ran)
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
                        // Truly external import — remove (will be hoisted)
                        edits.push(TextEdit {
                            start: import.span.start,
                            end: import.span.end,
                            replacement: None,
                        });
                    }
                }
                ModuleDeclaration::ExportNamedDeclaration(export) => {
                    if export.source.is_some() {
                        // Re-export: export { X } from './other'
                        let source = export
                            .source
                            .as_ref()
                            .map(|s| s.value.as_str())
                            .unwrap_or("");
                        let target_ns = resolve_import(source);
                        let mut replacements = Vec::new();
                        for spec in &export.specifiers {
                            let exported = spec.exported.name().to_string();
                            let local = spec.local.name().to_string();
                            exported_names.push(exported.clone());
                            if let Some(ref ns) = target_ns {
                                replacements.push(format!("var {exported} = {ns}.{local};"));
                            }
                        }
                        let replacement = if replacements.is_empty() {
                            None
                        } else {
                            Some(replacements.join("\n"))
                        };
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.end,
                            replacement,
                        });
                    } else if export.declaration.is_some() {
                        // export const X = ...; → strip "export "
                        if let Some(decl) = &export.declaration {
                            collect_decl_names(decl, &mut exported_names);
                        }
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.start + 7, // "export "
                            replacement: None,
                        });
                    } else {
                        // export { X, Y }; → collect names and remove
                        for spec in &export.specifiers {
                            exported_names.push(spec.exported.name().to_string());
                        }
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.end,
                            replacement: None,
                        });
                    }
                }
                ModuleDeclaration::ExportDefaultDeclaration(export) => {
                    has_default_export = true;
                    match &export.declaration {
                        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                            if let Some(id) = &f.id {
                                exported_names.push(id.name.to_string());
                            }
                            // Strip "export default "
                            edits.push(TextEdit {
                                start: export.span.start,
                                end: export.span.start + 15,
                                replacement: None,
                            });
                        }
                        ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                            if let Some(id) = &c.id {
                                exported_names.push(id.name.to_string());
                            }
                            edits.push(TextEdit {
                                start: export.span.start,
                                end: export.span.start + 15,
                                replacement: None,
                            });
                        }
                        _ => {
                            // export default <expr>; → __exports.default = <expr>;
                            edits.push(TextEdit {
                                start: export.span.start,
                                end: export.span.start + 15, // "export default "
                                replacement: Some("__exports.default = ".to_string()),
                            });
                        }
                    }
                }
                ModuleDeclaration::ExportAllDeclaration(export) => {
                    let source = export.source.value.as_str();
                    if let Some(target_ns) = resolve_import(source) {
                        // export * from './other' → Object.assign(__exports, ns)
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.end,
                            replacement: Some(format!("Object.assign(__exports, {target_ns});")),
                        });
                    } else {
                        edits.push(TextEdit {
                            start: export.span.start,
                            end: export.span.end,
                            replacement: None,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // Apply edits
    let module_code = apply_edits(js_code, &mut edits);

    // Build export assignments
    let mut export_lines = String::new();
    let unique_exports: BTreeSet<String> = exported_names.iter().cloned().collect();
    for name in &unique_exports {
        // Skip "default" — it's a reserved word and can't be used as an identifier.
        // Default exports are handled inline (expression → __exports.default = expr)
        // or via the named function/class assignment below.
        if name == "default" {
            continue;
        }
        export_lines.push_str(&format!("  __exports.{name} = {name};\n"));
    }
    if has_default_export {
        // For named default exports (export default function X / class X),
        // assign the named identifier to __exports.default
        for name in &exported_names {
            if name != "default" {
                if !export_lines.contains("__exports.default =") {
                    export_lines.push_str(&format!("  __exports.default = {name};\n"));
                }
                break;
            }
        }
    }

    // Wrap in IIFE
    let wrapped = format!(
        "var {namespace} = {{}};\n(function(__exports) {{\n{module_code}{export_lines}}})({namespace});"
    );

    // Validate the wrapped code parses correctly
    let allocator2 = Allocator::new();
    let check = Parser::new(&allocator2, &wrapped, SourceType::mjs()).parse();
    if check.panicked || !check.errors.is_empty() {
        // IIFE wrapping produced invalid JS — fall back to flat code with namespace population.
        // We still create the namespace variable and assign exports to it so other
        // IIFE-wrapped modules can reference this module's symbols via __ns_xxx.name.
        tracing::warn!(
            file_name,
            "IIFE wrapping produced parse errors, falling back to flat inclusion with namespace"
        );
        let flat_code = strip_exports_simple(js_code);
        let mut ns_assignments = format!("var {namespace} = {{}};\n");
        ns_assignments.push_str(&flat_code);
        ns_assignments.push('\n');
        for name in &unique_exports {
            if name == "default" {
                continue;
            }
            // Use typeof check to avoid ReferenceError for names that might not exist
            ns_assignments.push_str(&format!(
                "if (typeof {name} !== 'undefined') {namespace}.{name} = {name};\n"
            ));
        }
        return Ok(NpmModuleInfo {
            wrapped_code: ns_assignments,
            exported_names: unique_exports.into_iter().collect(),
        });
    }

    Ok(NpmModuleInfo {
        wrapped_code: wrapped,
        exported_names: unique_exports.into_iter().collect(),
    })
}

/// Generate a namespace variable name from a file path.
///
/// Sanitizes the path into a valid JS identifier: `__ns_angular_core_fesm2022_core`.
pub fn namespace_from_path(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let s = relative.to_string_lossy();
    let sanitized: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Trim leading underscores and ensure it starts with __ns_
    let trimmed = sanitized.trim_start_matches('_');
    // Take last ~40 chars to keep it reasonable
    let short = if trimmed.len() > 40 {
        &trimmed[trimmed.len() - 40..]
    } else {
        trimmed
    };
    format!("__ns_{short}")
}

/// Split a bare specifier into package name and subpath.
///
/// `@angular/core` → (`@angular/core`, `.`)
/// `@angular/core/testing` → (`@angular/core`, `./testing`)
/// `rxjs` → (`rxjs`, `.`)
/// `rxjs/operators` → (`rxjs`, `./operators`)
pub fn split_package_name(specifier: &str) -> (String, String) {
    crate::npm_wrap::package_json_split(specifier)
}

/// Internal helper to split specifier (delegates to package_json logic).
fn package_json_split(specifier: &str) -> (String, String) {
    if specifier.starts_with('@') {
        let parts: Vec<&str> = specifier.splitn(3, '/').collect();
        if parts.len() >= 3 {
            (
                format!("{}/{}", parts[0], parts[1]),
                format!("./{}", parts[2]),
            )
        } else {
            (specifier.to_string(), ".".to_string())
        }
    } else {
        let parts: Vec<&str> = specifier.splitn(2, '/').collect();
        if parts.len() == 2 {
            (parts[0].to_string(), format!("./{}", parts[1]))
        } else {
            (specifier.to_string(), ".".to_string())
        }
    }
}

/// Simple fallback: strip export keywords from source code without IIFE wrapping.
///
/// Used when the full IIFE wrapping produces invalid JS. This is less safe
/// (no scope isolation) but produces valid code.
fn strip_exports_simple(source: &str) -> String {
    let re_export_default = regex::Regex::new(r"(?m)^export default ").expect("valid regex");
    let re_export_keyword =
        regex::Regex::new(r"(?m)^export (?:const |let |var |function |class |async function )")
            .expect("valid regex");
    let re_export_list = regex::Regex::new(r"(?m)^export \{[^}]*\};?\s*$").expect("valid regex");

    let result = re_export_list.replace_all(source, "");
    let result = re_export_default.replace_all(&result, "");
    // For "export const/let/var/function/class", keep the declaration, strip "export "
    let result = re_export_keyword.replace_all(&result, |caps: &regex::Captures| {
        caps[0]
            .strip_prefix("export ")
            .unwrap_or(&caps[0])
            .to_string()
    });
    result.to_string()
}

/// A text edit to apply to the source.
struct TextEdit {
    start: u32,
    end: u32,
    replacement: Option<String>,
}

/// Apply text edits to source code.
fn apply_edits(source: &str, edits: &mut [TextEdit]) -> String {
    edits.sort_by(|a, b| b.start.cmp(&a.start));

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

/// Collect declaration names from an AST declaration.
fn collect_decl_names(decl: &oxc_ast::ast::Declaration, names: &mut Vec<String>) {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(var) => {
            for declarator in &var.declarations {
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    names.push(id.name.to_string());
                }
            }
        }
        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                names.push(id.name.to_string());
            }
        }
        oxc_ast::ast::Declaration::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                names.push(id.name.to_string());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_resolve(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn test_wrap_simple_module() {
        let code = "export function hello() { return 42; }\n";
        let result = wrap_npm_module(code, "test.js", "__ns_test", no_resolve).unwrap();
        assert!(result.wrapped_code.contains("var __ns_test = {};"));
        assert!(result.wrapped_code.contains("(function(__exports)"));
        assert!(result.wrapped_code.contains("__exports.hello = hello;"));
        assert!(result.wrapped_code.contains("function hello()"));
        assert!(!result.wrapped_code.contains("export function"));
    }

    #[test]
    fn test_wrap_with_import_rewrite() {
        let code =
            "import { Component } from '@angular/core';\nexport class MyClass extends Component {}\n";
        let resolve = |spec: &str| -> Option<String> {
            if spec == "@angular/core" {
                Some("__ns_core".to_string())
            } else {
                None
            }
        };
        let result = wrap_npm_module(code, "test.js", "__ns_test", resolve).unwrap();
        assert!(result
            .wrapped_code
            .contains("var Component = __ns_core.Component;"));
        assert!(!result.wrapped_code.contains("from '@angular/core'"));
        assert!(result.wrapped_code.contains("__exports.MyClass = MyClass;"));
    }

    #[test]
    fn test_wrap_reexport() {
        let code = "export * from './utils';\n";
        let resolve = |spec: &str| -> Option<String> {
            if spec == "./utils" {
                Some("__ns_utils".to_string())
            } else {
                None
            }
        };
        let result = wrap_npm_module(code, "test.js", "__ns_test", resolve).unwrap();
        assert!(result
            .wrapped_code
            .contains("Object.assign(__exports, __ns_utils)"));
    }

    #[test]
    fn test_wrap_default_export() {
        let code = "export default function helper() { return 1; }\n";
        let result = wrap_npm_module(code, "test.js", "__ns_test", no_resolve).unwrap();
        assert!(result.wrapped_code.contains("function helper()"));
        assert!(result.wrapped_code.contains("__exports.default = helper;"));
    }

    #[test]
    fn test_namespace_from_path() {
        use std::path::PathBuf;
        let ns = namespace_from_path(
            &PathBuf::from("/root/node_modules/@angular/core/fesm2022/core.mjs"),
            &PathBuf::from("/root/node_modules"),
        );
        assert!(ns.starts_with("__ns_"));
        assert!(ns.contains("core"));
    }
}
