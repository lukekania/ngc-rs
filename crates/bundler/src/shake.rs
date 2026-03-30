//! Tree shaking: export-level dead code elimination.
//!
//! Analyzes which exports from each module are actually imported by other modules
//! in the same chunk. Exports that are never referenced can be removed, and modules
//! with no used exports and no side effects can be dropped entirely.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ExportDefaultDeclarationKind, ImportDeclarationSpecifier, ModuleDeclaration, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use tracing::debug;

/// Information about a module's exports and imports for tree shaking analysis.
struct ModuleInfo {
    /// Names exported by this module.
    exported_names: HashSet<String>,
    /// Names imported from local modules: maps source specifier -> set of imported names.
    local_imports: HashMap<String, HashSet<String>>,
    /// Whether this module has top-level side effects (expression statements, etc.).
    has_side_effects: bool,
}

/// Analyze export usage across modules in a chunk and return unused exports per module.
///
/// Returns a map from module path to the set of export names that are never imported
/// by any other module in the chunk. The entry module's exports are always considered used.
///
/// Modules with no used exports, no side effects, and that are not the entry point
/// are indicated by having ALL their exports listed as unused — the caller can
/// choose to drop them entirely.
pub fn analyze_unused_exports(
    module_paths: &[PathBuf],
    all_code: &HashMap<PathBuf, String>,
    entry: &PathBuf,
    local_prefixes: &[&str],
) -> NgcResult<HashMap<PathBuf, HashSet<String>>> {
    // Step 1: Parse each module and collect export/import information
    let mut module_infos: HashMap<PathBuf, ModuleInfo> = HashMap::new();

    for module_path in module_paths {
        let code = match all_code.get(module_path) {
            Some(c) => c,
            None => continue,
        };

        let info = analyze_module(code, module_path)?;
        module_infos.insert(module_path.clone(), info);
    }

    // Step 2: Build usage map — which exports are actually referenced
    let mut used_exports: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    // Entry module exports are always considered used
    if let Some(info) = module_infos.get(entry) {
        used_exports
            .entry(entry.clone())
            .or_default()
            .extend(info.exported_names.iter().cloned());
    }

    // For each module, check what it imports from other local modules
    for (importer_path, info) in &module_infos {
        for (specifier, imported_names) in &info.local_imports {
            // Resolve specifier to a module path
            if let Some(target_path) =
                resolve_local_specifier(specifier, importer_path, module_paths, local_prefixes)
            {
                debug!(
                    importer = %importer_path.display(),
                    specifier = specifier,
                    target = %target_path.display(),
                    names = ?imported_names,
                    "tree shake: resolved import"
                );
                used_exports
                    .entry(target_path)
                    .or_default()
                    .extend(imported_names.iter().cloned());
            }
        }
    }

    // Step 3: Compute unused exports
    let mut unused: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    for (module_path, info) in &module_infos {
        // Skip entry module — its exports are always kept
        if module_path == entry {
            continue;
        }

        // Skip modules with side effects — they must be kept
        if info.has_side_effects {
            continue;
        }

        let used = used_exports.get(module_path);
        let mut unused_names = HashSet::new();

        for name in &info.exported_names {
            let is_used = used.is_some_and(|u| u.contains(name));
            if !is_used {
                unused_names.insert(name.clone());
            }
        }

        if !unused_names.is_empty() {
            debug!(
                module = %module_path.display(),
                unused_count = unused_names.len(),
                "tree shake: found unused exports"
            );
            unused.insert(module_path.clone(), unused_names);
        }
    }

    Ok(unused)
}

/// Parse a module and extract export/import information for tree shaking.
fn analyze_module(code: &str, path: &Path) -> NgcResult<ModuleInfo> {
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();

    if parsed.panicked {
        return Err(NgcError::BundleError {
            message: format!("tree shake parse failed for {}", path.display()),
        });
    }

    let mut exported_names = HashSet::new();
    let mut local_imports: HashMap<String, HashSet<String>> = HashMap::new();
    let mut has_side_effects = false;

    for stmt in &parsed.program.body {
        if matches!(stmt, Statement::ExpressionStatement(_)) {
            has_side_effects = true;
        }

        if let Some(module_decl) = stmt.as_module_declaration() {
            match module_decl {
                ModuleDeclaration::ImportDeclaration(import) => {
                    let source = import.source.value.to_string();
                    if let Some(specifiers) = &import.specifiers {
                        let names: HashSet<String> = specifiers
                            .iter()
                            .filter_map(|spec| match spec {
                                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                    Some(s.local.name.to_string())
                                }
                                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                                    Some("default".to_string())
                                }
                                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => None,
                            })
                            .collect();
                        if !names.is_empty() {
                            local_imports.entry(source).or_default().extend(names);
                        }
                    } else {
                        // Side-effect import: import 'foo'
                        has_side_effects = true;
                    }
                }
                ModuleDeclaration::ExportNamedDeclaration(export) => {
                    if let Some(decl) = &export.declaration {
                        collect_declaration_names(decl, &mut exported_names);
                    }
                    for spec in &export.specifiers {
                        exported_names.insert(spec.exported.name().to_string());
                    }
                }
                ModuleDeclaration::ExportDefaultDeclaration(export) => {
                    exported_names.insert("default".to_string());
                    match &export.declaration {
                        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                            if let Some(id) = &f.id {
                                exported_names.insert(id.name.to_string());
                            }
                        }
                        ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                            if let Some(id) = &c.id {
                                exported_names.insert(id.name.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                ModuleDeclaration::ExportAllDeclaration(_) => {
                    // Re-export everything — treat as side-effectful (can't analyze)
                    has_side_effects = true;
                }
                _ => {}
            }
        }
    }

    Ok(ModuleInfo {
        exported_names,
        local_imports,
        has_side_effects,
    })
}

/// Collect declared names from a declaration.
fn collect_declaration_names(decl: &oxc_ast::ast::Declaration, names: &mut HashSet<String>) {
    match decl {
        oxc_ast::ast::Declaration::VariableDeclaration(var) => {
            for declarator in &var.declarations {
                if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &declarator.id {
                    names.insert(id.name.to_string());
                }
            }
        }
        oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                names.insert(id.name.to_string());
            }
        }
        oxc_ast::ast::Declaration::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                names.insert(id.name.to_string());
            }
        }
        _ => {}
    }
}

/// Try to resolve a local import specifier to a module path.
///
/// This is a best-effort resolution — it checks if the specifier starts with
/// a local prefix and tries to find a matching module path.
fn resolve_local_specifier(
    specifier: &str,
    importer: &Path,
    module_paths: &[PathBuf],
    local_prefixes: &[&str],
) -> Option<PathBuf> {
    let is_local = local_prefixes.iter().any(|p| specifier.starts_with(p));
    if !is_local {
        return None;
    }

    // For relative imports, resolve against the importer's directory
    let importer_dir = importer.parent()?;

    // Try various extensions and index file patterns
    let candidates: Vec<PathBuf> = if specifier.starts_with('.') {
        let base = importer_dir.join(specifier);
        vec![
            base.clone(),
            base.with_extension("ts"),
            base.with_extension("tsx"),
            base.with_extension("js"),
            base.join("index.ts"),
            base.join("index.js"),
        ]
    } else {
        // Path alias — can't resolve without alias mapping, skip
        return None;
    };

    for candidate in &candidates {
        if let Ok(canonical) = candidate.canonicalize() {
            if module_paths.contains(&canonical) {
                return Some(canonical);
            }
        }
    }

    // Fallback: try suffix matching against module paths
    for module_path in module_paths {
        let module_str = module_path.to_string_lossy();
        // Strip leading ./ and try matching
        let spec_clean = specifier.strip_prefix("./").unwrap_or(specifier);
        if module_str.ends_with(spec_clean)
            || module_str.ends_with(&format!("{spec_clean}.ts"))
            || module_str.ends_with(&format!("{spec_clean}.js"))
            || module_str.ends_with(&format!("{spec_clean}/index.ts"))
        {
            return Some(module_path.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unused_export_detected() {
        let mut modules = HashMap::new();
        // Use JS code (already transformed) since the bundler operates on JS
        modules.insert(
            PathBuf::from("/root/utils.js"),
            "export const used = 1;\nexport const unused = 2;\n".to_string(),
        );
        modules.insert(
            PathBuf::from("/root/main.js"),
            "import { used } from './utils';\nconsole.log(used);\n".to_string(),
        );

        let paths = vec![
            PathBuf::from("/root/utils.js"),
            PathBuf::from("/root/main.js"),
        ];

        let result =
            analyze_unused_exports(&paths, &modules, &PathBuf::from("/root/main.js"), &["."])
                .expect("should analyze");

        let utils_unused = result.get(&PathBuf::from("/root/utils.js"));
        assert!(utils_unused.is_some(), "utils should have unused exports");
        assert!(
            utils_unused.expect("checked").contains("unused"),
            "unused export should be detected"
        );
        assert!(
            !utils_unused.expect("checked").contains("used"),
            "used export should not be in unused set"
        );
    }

    #[test]
    fn test_entry_exports_always_kept() {
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/root/main.ts"),
            "export const x = 1;\nexport const y = 2;\n".to_string(),
        );

        let paths = vec![PathBuf::from("/root/main.ts")];

        let result =
            analyze_unused_exports(&paths, &modules, &PathBuf::from("/root/main.ts"), &["."])
                .expect("should analyze");

        assert!(
            !result.contains_key(&PathBuf::from("/root/main.ts")),
            "entry module exports should never be marked unused"
        );
    }

    #[test]
    fn test_side_effect_module_kept() {
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/root/side.ts"),
            "export const x = 1;\nconsole.log('side effect');\n".to_string(),
        );
        modules.insert(
            PathBuf::from("/root/main.ts"),
            "import './side';\n".to_string(),
        );

        let paths = vec![
            PathBuf::from("/root/side.ts"),
            PathBuf::from("/root/main.ts"),
        ];

        let result =
            analyze_unused_exports(&paths, &modules, &PathBuf::from("/root/main.ts"), &["."])
                .expect("should analyze");

        assert!(
            !result.contains_key(&PathBuf::from("/root/side.ts")),
            "side-effect module should not have unused exports listed"
        );
    }
}
