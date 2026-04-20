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
///
/// `externally_used` optionally carries a flat set of names that must be preserved
/// across every module in the chunk regardless of intra-chunk usage. Callers pass
/// this for the main chunk to reflect symbols consumed cross-chunk by lazy chunks
/// — such consumption is invisible to the per-chunk analysis below and would
/// otherwise leave dangling names in the bundler's final `export { ... }` block.
pub fn analyze_unused_exports(
    module_paths: &[PathBuf],
    all_code: &HashMap<PathBuf, String>,
    entry: &PathBuf,
    local_prefixes: &[&str],
    externally_used: Option<&HashSet<String>>,
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
            let is_externally_used = externally_used.is_some_and(|set| set.contains(name));
            if !is_used && !is_externally_used {
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

    // Try various extensions and index file patterns.
    //
    // Append extensions by string concatenation rather than `Path::with_extension`,
    // because filenames like `analytics.service` contain a dot that would
    // otherwise be treated as an existing extension and replaced.
    let candidates: Vec<PathBuf> = if specifier.starts_with('.') {
        let base = importer_dir.join(specifier);
        let base_str = base.to_string_lossy().into_owned();
        vec![
            base.clone(),
            PathBuf::from(format!("{base_str}.ts")),
            PathBuf::from(format!("{base_str}.tsx")),
            PathBuf::from(format!("{base_str}.js")),
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

/// Collect symbol names imported by `consumer_modules` from any module in
/// `provider_modules`, by parsing each consumer's source for `ImportDeclaration`
/// statements and resolving their specifiers against the provider set.
///
/// Used by the bundler to tell the main-chunk tree-shaker which symbols are
/// consumed by lazy chunks and must therefore be preserved — such consumption
/// is invisible when shaking each chunk in isolation, and would otherwise
/// leave the cross-chunk `export { ... }` block referring to names whose
/// declarations have been tree-shaken away.
///
/// For named and default imports, the specific name is collected. For
/// namespace imports (`import * as X from '...'`), every exported name of
/// the target provider module is collected since individual accesses can't
/// be known statically here.
pub fn collect_cross_chunk_used_names(
    consumer_modules: &[PathBuf],
    provider_modules: &[PathBuf],
    all_code: &HashMap<PathBuf, String>,
    local_prefixes: &[&str],
) -> NgcResult<HashSet<String>> {
    let mut used: HashSet<String> = HashSet::new();
    let provider_set: HashSet<&PathBuf> = provider_modules.iter().collect();

    // Cache provider exports so we only parse each once when expanding namespace imports.
    let mut provider_exports: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    for consumer_path in consumer_modules {
        let Some(code) = all_code.get(consumer_path) else {
            continue;
        };

        let info = analyze_module(code, consumer_path)?;
        for (specifier, imported_names) in &info.local_imports {
            let Some(target) =
                resolve_local_specifier(specifier, consumer_path, provider_modules, local_prefixes)
            else {
                continue;
            };

            if !provider_set.contains(&target) {
                continue;
            }

            for name in imported_names {
                if name == "* as " || name.starts_with("* as ") {
                    // ImportNamespaceSpecifier — `analyze_module` currently drops these
                    // (returns None), so this branch is defensive. Fall through to the
                    // dedicated namespace pass below when the signal is present.
                    continue;
                }
                used.insert(name.clone());
            }
        }

        // ImportNamespaceSpecifier is not captured by `analyze_module` (which
        // returns None for namespace imports). Re-parse here to expand them
        // into the full provider export set for each such import.
        expand_namespace_imports(
            code,
            consumer_path,
            provider_modules,
            local_prefixes,
            all_code,
            &mut provider_exports,
            &mut used,
        )?;
    }

    Ok(used)
}

fn expand_namespace_imports(
    code: &str,
    consumer_path: &Path,
    provider_modules: &[PathBuf],
    local_prefixes: &[&str],
    all_code: &HashMap<PathBuf, String>,
    provider_exports: &mut HashMap<PathBuf, HashSet<String>>,
    used: &mut HashSet<String>,
) -> NgcResult<()> {
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if parsed.panicked {
        return Ok(());
    }

    for stmt in &parsed.program.body {
        let Some(ModuleDeclaration::ImportDeclaration(import)) = stmt.as_module_declaration()
        else {
            continue;
        };
        let Some(specifiers) = &import.specifiers else {
            continue;
        };
        let has_namespace = specifiers
            .iter()
            .any(|s| matches!(s, ImportDeclarationSpecifier::ImportNamespaceSpecifier(_)));
        if !has_namespace {
            continue;
        }
        let source = import.source.value.to_string();
        let Some(target) =
            resolve_local_specifier(&source, consumer_path, provider_modules, local_prefixes)
        else {
            continue;
        };
        let exports = match provider_exports.get(&target) {
            Some(e) => e.clone(),
            None => {
                let Some(provider_code) = all_code.get(&target) else {
                    continue;
                };
                let info = analyze_module(provider_code, &target)?;
                provider_exports.insert(target.clone(), info.exported_names.clone());
                info.exported_names
            }
        };
        used.extend(exports);
    }

    Ok(())
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

        let result = analyze_unused_exports(
            &paths,
            &modules,
            &PathBuf::from("/root/main.js"),
            &["."],
            None,
        )
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

        let result = analyze_unused_exports(
            &paths,
            &modules,
            &PathBuf::from("/root/main.ts"),
            &["."],
            None,
        )
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

        let result = analyze_unused_exports(
            &paths,
            &modules,
            &PathBuf::from("/root/main.ts"),
            &["."],
            None,
        )
        .expect("should analyze");

        assert!(
            !result.contains_key(&PathBuf::from("/root/side.ts")),
            "side-effect module should not have unused exports listed"
        );
    }

    #[test]
    fn test_externally_used_preserves_export() {
        // When a name is marked externally used (e.g. consumed by a lazy
        // chunk cross-chunk), it must not be reported as unused even if
        // no intra-chunk importer references it.
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/root/svc.js"),
            "export class AnalyticsService {}\n".to_string(),
        );
        modules.insert(
            PathBuf::from("/root/main.js"),
            "// main has no import of AnalyticsService\n".to_string(),
        );

        let paths = vec![
            PathBuf::from("/root/svc.js"),
            PathBuf::from("/root/main.js"),
        ];

        let mut externally_used = HashSet::new();
        externally_used.insert("AnalyticsService".to_string());

        let result = analyze_unused_exports(
            &paths,
            &modules,
            &PathBuf::from("/root/main.js"),
            &["."],
            Some(&externally_used),
        )
        .expect("should analyze");

        let svc_unused = result.get(&PathBuf::from("/root/svc.js"));
        assert!(
            svc_unused.is_none() || !svc_unused.expect("checked").contains("AnalyticsService"),
            "externally-used export must not be flagged unused"
        );
    }

    #[test]
    fn test_collect_cross_chunk_used_names_dotted_filename() {
        // Regression: resolve_local_specifier previously used `with_extension`,
        // which treated `.service` as an existing extension and replaced it.
        // Imports like `./foo.service` then failed to resolve against
        // `foo.service.ts` and cross-chunk consumption was missed.
        let dir = tempfile::tempdir().expect("create temp dir");
        let svc = dir.path().join("analytics.service.ts");
        let comp = dir.path().join("comp.ts");
        std::fs::write(&svc, "export class AnalyticsService {}\n").expect("write svc");
        std::fs::write(
            &comp,
            "import { AnalyticsService } from './analytics.service';\nnew AnalyticsService();\n",
        )
        .expect("write comp");

        let canon_svc = svc.canonicalize().expect("canon svc");
        let canon_comp = comp.canonicalize().expect("canon comp");

        let mut modules = HashMap::new();
        modules.insert(
            canon_svc.clone(),
            "export class AnalyticsService {}\n".into(),
        );
        modules.insert(
            canon_comp.clone(),
            "import { AnalyticsService } from './analytics.service';\nnew AnalyticsService();\n"
                .into(),
        );

        let used = collect_cross_chunk_used_names(&[canon_comp], &[canon_svc], &modules, &["."])
            .expect("should collect");
        assert!(
            used.contains("AnalyticsService"),
            "import of ./foo.service must resolve to foo.service.ts"
        );
    }

    #[test]
    fn test_collect_cross_chunk_used_names_named_import() {
        // A lazy-chunk module imports AnalyticsService from a main-chunk module;
        // collect_cross_chunk_used_names must surface it. Uses a real tempdir
        // so resolve_local_specifier's canonicalize step can succeed.
        let dir = tempfile::tempdir().expect("create temp dir");
        let main_svc = dir.path().join("svc.js");
        let lazy_dir = dir.path().join("lazy");
        std::fs::create_dir_all(&lazy_dir).expect("create lazy dir");
        let lazy_comp = lazy_dir.join("comp.js");
        std::fs::write(&main_svc, "export class AnalyticsService {}\n").expect("write svc");
        std::fs::write(
            &lazy_comp,
            "import { AnalyticsService } from '../svc';\nnew AnalyticsService();\n",
        )
        .expect("write comp");

        let canon_svc = main_svc.canonicalize().expect("canon svc");
        let canon_comp = lazy_comp.canonicalize().expect("canon comp");

        let mut modules = HashMap::new();
        modules.insert(
            canon_svc.clone(),
            "export class AnalyticsService {}\n".into(),
        );
        modules.insert(
            canon_comp.clone(),
            "import { AnalyticsService } from '../svc';\nnew AnalyticsService();\n".into(),
        );

        let used = collect_cross_chunk_used_names(&[canon_comp], &[canon_svc], &modules, &["."])
            .expect("should collect");
        assert!(used.contains("AnalyticsService"));
    }
}
