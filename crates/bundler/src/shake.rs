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
use rayon::prelude::*;
use tracing::debug;

/// Context for resolving `#`-prefixed subpath imports against the project's
/// `package.json` `imports` field. When `None`, `#`-aliased specifiers are
/// not resolvable and their use-edges are ignored (matching the pre-#92
/// behavior — used by tests that don't construct a real project).
#[derive(Clone, Copy)]
pub struct SubpathImportContext<'a> {
    pub root_dir: &'a Path,
    pub export_conditions: &'a [&'a str],
}

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
    subpath_ctx: Option<SubpathImportContext<'_>>,
) -> NgcResult<HashMap<PathBuf, HashSet<String>>> {
    // Step 1: Parse each module in parallel and collect export/import info.
    // `analyze_module` only reads its inputs, so per-module work is independent.
    let module_infos: HashMap<PathBuf, ModuleInfo> = module_paths
        .par_iter()
        .filter_map(|path| all_code.get(path).map(|code| (path, code)))
        .map(|(path, code)| -> NgcResult<(PathBuf, ModuleInfo)> {
            let info = analyze_module(code, path)?;
            Ok((path.clone(), info))
        })
        .collect::<NgcResult<HashMap<_, _>>>()?;

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
            if let Some(target_path) = resolve_local_specifier(
                specifier,
                importer_path,
                module_paths,
                local_prefixes,
                subpath_ctx,
            ) {
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
/// a local prefix and tries to find a matching module path. When `subpath_ctx`
/// is provided, `#`-prefixed specifiers are also resolved through the
/// project's `package.json` `imports` field — without this, a class field
/// initializer reading a value from a `#`-aliased module (issue #92) is not
/// recognized as a use of the target's export, the declaration gets shaken
/// out, and the class throws `ReferenceError` at instance construction.
fn resolve_local_specifier(
    specifier: &str,
    importer: &Path,
    module_paths: &[PathBuf],
    local_prefixes: &[&str],
    subpath_ctx: Option<SubpathImportContext<'_>>,
) -> Option<PathBuf> {
    if specifier.starts_with('#') {
        let ctx = subpath_ctx?;
        let resolved = ngc_npm_resolver::resolve::resolve_subpath_import(
            specifier,
            Some(importer),
            ctx.root_dir,
            ctx.export_conditions,
        )
        .ok()?;
        let canonical = resolved.canonicalize().unwrap_or(resolved);
        return module_paths.iter().find(|p| **p == canonical).cloned();
    }

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

/// Per-provider collection of names consumed cross-chunk.
///
/// Returns a `Vec<HashSet<String>>` indexed by chunk index. `result[i]` holds
/// the set of names that modules in *other* chunks import from any module
/// owned by chunk `i`. Used by the bundler's per-chunk tree-shaker so a
/// vendor chunk holding `@angular/core` / `rxjs` can drop exports no
/// consumer references, instead of pinning every name the package declares
/// just because its entry walk happens to reach them.
///
/// `specifier_to_path` resolves bare npm specifiers (`'@angular/core'`) to
/// the canonical entry-module path so bare-specifier imports can be
/// attributed to their owning provider chunk. Relative and `#`-subpath
/// imports flow through [`resolve_local_specifier`] as usual.
pub fn collect_cross_chunk_used_names_per_provider(
    chunk_graph: &crate::chunk::ChunkGraph,
    all_code: &HashMap<PathBuf, String>,
    local_prefixes: &[&str],
    specifier_to_path: &HashMap<String, PathBuf>,
    subpath_ctx: Option<SubpathImportContext<'_>>,
) -> NgcResult<Vec<HashSet<String>>> {
    let n = chunk_graph.chunks.len();
    let mut result: Vec<HashSet<String>> = vec![HashSet::new(); n];
    if n == 0 {
        return Ok(result);
    }

    let module_to_chunk_idx = &chunk_graph.module_to_chunk_idx;

    // Flat (consumer_chunk_idx, consumer_path) list — every module across
    // every chunk is a potential consumer of names in some other chunk.
    let consumers: Vec<(usize, PathBuf)> = chunk_graph
        .chunks
        .iter()
        .enumerate()
        .flat_map(|(idx, chunk)| chunk.modules.iter().map(move |m| (idx, m.clone())))
        .collect();

    // The full provider candidate set — every module across all chunks.
    // `resolve_local_specifier` scans this when matching a relative import
    // path; chunk membership is then read from `module_to_chunk_idx`.
    let all_provider_paths: Vec<PathBuf> = consumers.iter().map(|(_, p)| p.clone()).collect();

    // Phase A (parallel): for each consumer, parse once and produce a list
    // of (target_chunk_idx, imported_name) entries. Intra-chunk imports
    // are dropped here — those are handled by `analyze_unused_exports`'s
    // per-chunk reachability pass.
    let per_consumer: Vec<Vec<(usize, String)>> = consumers
        .par_iter()
        .filter_map(|(consumer_idx, consumer_path)| {
            all_code
                .get(consumer_path)
                .map(|code| (*consumer_idx, consumer_path.clone(), code))
        })
        .map(
            |(consumer_idx, consumer_path, code)| -> NgcResult<Vec<(usize, String)>> {
                let info = analyze_module(code, &consumer_path)?;
                let mut out: Vec<(usize, String)> = Vec::new();
                for (specifier, imported_names) in &info.local_imports {
                    let target_path = resolve_local_specifier(
                        specifier,
                        &consumer_path,
                        &all_provider_paths,
                        local_prefixes,
                        subpath_ctx,
                    )
                    .or_else(|| specifier_to_path.get(specifier).cloned());

                    let Some(target) = target_path else {
                        continue;
                    };
                    let Some(&target_idx) = module_to_chunk_idx.get(&target) else {
                        continue;
                    };
                    if target_idx == consumer_idx {
                        continue;
                    }
                    for name in imported_names {
                        out.push((target_idx, name.clone()));
                    }
                }
                Ok(out)
            },
        )
        .collect::<NgcResult<Vec<_>>>()?;

    for entries in per_consumer {
        for (idx, name) in entries {
            if let Some(set) = result.get_mut(idx) {
                set.insert(name);
            }
        }
    }

    // Phase B (serial): namespace-import expansion. Each `import * as X
    // from '...'` in a consumer adds every export of the target module to
    // the owning chunk's used set. Provider parses are cached so a hot
    // namespace import is parsed at most once.
    let mut provider_exports: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for (consumer_idx, consumer_path) in &consumers {
        let Some(code) = all_code.get(consumer_path) else {
            continue;
        };
        expand_namespace_imports_per_provider(
            code,
            consumer_path,
            *consumer_idx,
            &all_provider_paths,
            module_to_chunk_idx,
            specifier_to_path,
            local_prefixes,
            all_code,
            &mut provider_exports,
            &mut result,
            subpath_ctx,
        )?;
    }

    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn expand_namespace_imports_per_provider(
    code: &str,
    consumer_path: &Path,
    consumer_chunk_idx: usize,
    provider_modules: &[PathBuf],
    module_to_chunk_idx: &HashMap<PathBuf, usize>,
    specifier_to_path: &HashMap<String, PathBuf>,
    local_prefixes: &[&str],
    all_code: &HashMap<PathBuf, String>,
    provider_exports: &mut HashMap<PathBuf, HashSet<String>>,
    per_chunk_used: &mut [HashSet<String>],
    subpath_ctx: Option<SubpathImportContext<'_>>,
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
        let target = resolve_local_specifier(
            &source,
            consumer_path,
            provider_modules,
            local_prefixes,
            subpath_ctx,
        )
        .or_else(|| specifier_to_path.get(&source).cloned());
        let Some(target) = target else { continue };
        let Some(&target_idx) = module_to_chunk_idx.get(&target) else {
            continue;
        };
        if target_idx == consumer_chunk_idx {
            continue;
        }

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
        if let Some(set) = per_chunk_used.get_mut(target_idx) {
            set.extend(exports);
        }
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
            None,
        )
        .expect("should analyze");

        let svc_unused = result.get(&PathBuf::from("/root/svc.js"));
        assert!(
            svc_unused.is_none() || !svc_unused.expect("checked").contains("AnalyticsService"),
            "externally-used export must not be flagged unused"
        );
    }

    #[test]
    fn test_collect_cross_chunk_used_names_per_provider_dotted_filename() {
        // Regression: resolve_local_specifier previously used `with_extension`,
        // which treated `.service` as an existing extension and replaced it.
        // Imports like `./foo.service` then failed to resolve against
        // `foo.service.ts` and cross-chunk consumption was missed.
        use crate::chunk::{Chunk, ChunkGraph, ChunkKind};

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

        let mut all_code = HashMap::new();
        all_code.insert(
            canon_svc.clone(),
            "export class AnalyticsService {}\n".into(),
        );
        all_code.insert(
            canon_comp.clone(),
            "import { AnalyticsService } from './analytics.service';\nnew AnalyticsService();\n"
                .into(),
        );

        let chunks = vec![
            Chunk {
                kind: ChunkKind::Main,
                filename: "main.js".to_string(),
                modules: vec![canon_svc.clone()],
                entry: canon_svc.clone(),
            },
            Chunk {
                kind: ChunkKind::Lazy,
                filename: "lazy.js".to_string(),
                modules: vec![canon_comp.clone()],
                entry: canon_comp.clone(),
            },
        ];
        let mut module_to_chunk_idx: HashMap<PathBuf, usize> = HashMap::new();
        for (idx, chunk) in chunks.iter().enumerate() {
            for m in &chunk.modules {
                module_to_chunk_idx.insert(m.clone(), idx);
            }
        }
        let chunk_graph = ChunkGraph {
            chunks,
            dynamic_import_map: HashMap::new(),
            module_to_chunk_idx,
        };

        let per_provider = collect_cross_chunk_used_names_per_provider(
            &chunk_graph,
            &all_code,
            &["."],
            &HashMap::new(),
            None,
        )
        .expect("should collect");
        assert!(
            per_provider[0].contains("AnalyticsService"),
            "import of ./foo.service must resolve to foo.service.ts: {per_provider:?}"
        );
    }

    #[test]
    fn test_collect_cross_chunk_used_names_per_provider_multi_chunk() {
        // Three chunks: main (chunk 0), lazy (chunk 1) sourced via dynamic
        // import, and a vendor chunk (chunk 2) providing an npm-style
        // module. The lazy chunk imports one name from main and one name
        // from vendor; main imports nothing externally. Per-provider
        // result must attribute each import to the correct chunk only.
        use crate::chunk::{Chunk, ChunkGraph, ChunkKind};

        let dir = tempfile::tempdir().expect("create temp dir");
        let main_path = dir.path().join("main.js");
        let svc_path = dir.path().join("svc.js");
        let lazy_path = dir.path().join("lazy.js");
        let vendor_path = dir.path().join("vendor_pkg.js");

        std::fs::write(&main_path, "// main entry\n").expect("write main");
        std::fs::write(&svc_path, "export class MainService {}\n").expect("write svc");
        std::fs::write(
            &lazy_path,
            "import { MainService } from './svc';\n\
             import { vendorFn } from 'vendor-pkg';\n\
             new MainService(); vendorFn();\n",
        )
        .expect("write lazy");
        std::fs::write(
            &vendor_path,
            "export const vendorFn = () => 1;\nexport const vendorUnused = () => 2;\n",
        )
        .expect("write vendor");

        let canon_main = main_path.canonicalize().expect("canon main");
        let canon_svc = svc_path.canonicalize().expect("canon svc");
        let canon_lazy = lazy_path.canonicalize().expect("canon lazy");
        let canon_vendor = vendor_path.canonicalize().expect("canon vendor");

        let mut all_code: HashMap<PathBuf, String> = HashMap::new();
        all_code.insert(canon_main.clone(), "// main entry\n".into());
        all_code.insert(canon_svc.clone(), "export class MainService {}\n".into());
        all_code.insert(
            canon_lazy.clone(),
            "import { MainService } from './svc';\n\
             import { vendorFn } from 'vendor-pkg';\n\
             new MainService(); vendorFn();\n"
                .into(),
        );
        all_code.insert(
            canon_vendor.clone(),
            "export const vendorFn = () => 1;\nexport const vendorUnused = () => 2;\n".into(),
        );

        let chunks = vec![
            Chunk {
                kind: ChunkKind::Main,
                filename: "main.js".to_string(),
                modules: vec![canon_main.clone(), canon_svc.clone()],
                entry: canon_main.clone(),
            },
            Chunk {
                kind: ChunkKind::Lazy,
                filename: "lazy.js".to_string(),
                modules: vec![canon_lazy.clone()],
                entry: canon_lazy.clone(),
            },
            Chunk {
                kind: ChunkKind::Shared,
                filename: "vendor.js".to_string(),
                modules: vec![canon_vendor.clone()],
                entry: canon_vendor.clone(),
            },
        ];
        let mut module_to_chunk_idx: HashMap<PathBuf, usize> = HashMap::new();
        for (idx, chunk) in chunks.iter().enumerate() {
            for m in &chunk.modules {
                module_to_chunk_idx.insert(m.clone(), idx);
            }
        }
        let chunk_graph = ChunkGraph {
            chunks,
            dynamic_import_map: HashMap::new(),
            module_to_chunk_idx,
        };

        let mut specifier_to_path: HashMap<String, PathBuf> = HashMap::new();
        specifier_to_path.insert("vendor-pkg".to_string(), canon_vendor.clone());

        let per_provider = collect_cross_chunk_used_names_per_provider(
            &chunk_graph,
            &all_code,
            &["."],
            &specifier_to_path,
            None,
        )
        .expect("should collect");

        assert_eq!(per_provider.len(), 3);
        assert!(
            per_provider[0].contains("MainService"),
            "lazy's import of MainService should land in main's set: {per_provider:?}"
        );
        assert!(
            !per_provider[0].contains("vendorFn"),
            "vendorFn must not be attributed to main"
        );
        assert!(
            per_provider[1].is_empty(),
            "no one imports from the lazy chunk; its set must be empty: {:?}",
            per_provider[1]
        );
        assert!(
            per_provider[2].contains("vendorFn"),
            "lazy's `import {{ vendorFn }} from 'vendor-pkg'` must land in vendor's set: {per_provider:?}"
        );
        assert!(
            !per_provider[2].contains("vendorUnused"),
            "vendorUnused is never imported — must not be in vendor's set"
        );
    }
}
