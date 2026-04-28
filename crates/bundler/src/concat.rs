use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use dashmap::DashMap;
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_project_resolver::ImportKind;
use oxc_sourcemap::{ConcatSourceMapBuilder, SourceMap};
use petgraph::graph::DiGraph;
use rayon::prelude::*;
use tracing::debug;

use crate::chunk::{build_chunk_graph, ChunkKind};
use crate::minify;
use crate::rewrite::{self, ExternalImport};
use crate::shake;

/// Shared, concurrent cache of `canonicalize()` results. The bundler probes
/// several candidate paths per import to resolve extensions and `index.*`
/// fallbacks; each probe is a `__getattrlist` syscall. Caching results
/// across rayon workers collapses duplicates to one syscall per unique
/// (attempted path → resolved path) pair.
type CanonCache = DashMap<PathBuf, Option<PathBuf>>;

/// Look up `p` in the cache, otherwise call `canonicalize()` and store the
/// result (successful resolution as `Some`, failure as `None`).
fn cached_canonicalize(cache: &CanonCache, p: &Path) -> Option<PathBuf> {
    if let Some(hit) = cache.get(p) {
        return hit.clone();
    }
    let canonical = p.canonicalize().ok();
    cache.insert(p.to_path_buf(), canonical.clone());
    canonical
}

/// Options controlling bundle output behavior.
#[derive(Debug, Clone, Copy, Default)]
pub struct BundleOptions {
    /// Generate source maps for bundled chunks.
    pub source_maps: bool,
    /// Minify the final output (whitespace removal).
    pub minify: bool,
    /// Use content-hash filenames for cache busting.
    pub content_hash: bool,
    /// Perform tree shaking (unused export elimination).
    pub tree_shake: bool,
}

/// Input to the bundler.
#[derive(Debug)]
pub struct BundleInput {
    /// Map from canonical source path to transformed JS code.
    pub modules: HashMap<PathBuf, String>,
    /// The file dependency graph (nodes are canonical paths, edges carry import kind).
    pub graph: DiGraph<PathBuf, ImportKind>,
    /// The entry point canonical path.
    pub entry: PathBuf,
    /// Prefixes that identify local imports (e.g. `["."]`, `[".", "@app/", "@env/"]`).
    pub local_prefixes: Vec<String>,
    /// Root directory for computing relative display paths in comments.
    pub root_dir: PathBuf,
    /// Build options controlling optimization and output behavior.
    pub options: BundleOptions,
    /// Per-module source maps from TS transform (keyed by canonical source path).
    /// Empty when source map generation is disabled.
    pub per_module_maps: HashMap<PathBuf, oxc_sourcemap::SourceMap>,
    /// Bare specifiers that have been resolved and included in the graph.
    /// The rewriter treats imports of these specifiers as local (strips them).
    pub bundled_specifiers: HashSet<String>,
    /// Active `exports` conditions (e.g. `browser`, `import`, `production`).
    /// Forwarded to the npm resolver when re-resolving specifiers during
    /// bundling so the same branch of conditional exports selected during
    /// dependency collection is selected again here.
    pub export_conditions: Vec<String>,
}

/// Merge result for a single source: all imports grouped.
struct MergedImport {
    source: String,
    default_import: Option<String>,
    named_imports: BTreeSet<String>,
    is_side_effect: bool,
}

/// The output of the bundler: one or more chunks.
#[derive(Debug)]
pub struct BundleOutput {
    /// Map from output filename to generated code.
    pub chunks: HashMap<String, String>,
    /// The main chunk filename (always `"main.js"` unless content-hashed).
    pub main_filename: String,
    /// Source maps for each chunk, keyed by the same filename as `chunks`.
    /// Empty when source map generation is disabled.
    pub chunk_source_maps: HashMap<String, SourceMap>,
}

/// Bundle all modules into one or more ESM chunk files.
///
/// Builds a chunk graph to detect code splitting boundaries from dynamic
/// `import()` expressions. For each chunk, topologically sorts its modules,
/// hoists and deduplicates external imports, strips local imports/exports,
/// and rewrites dynamic import specifiers to point to chunk filenames.
pub fn bundle(input: &BundleInput) -> NgcResult<BundleOutput> {
    let chunk_graph = build_chunk_graph(&input.graph, &input.entry, &input.root_dir)?;

    debug!(
        chunk_count = chunk_graph.chunks.len(),
        "bundling with code splitting"
    );

    // Build specifier-to-filename rewrite map for dynamic imports.
    // Maps the raw specifier as it appears in source code to the chunk filename.
    let specifier_rewrites = build_specifier_rewrite_map(input, &chunk_graph.dynamic_import_map)?;

    let prefix_refs: Vec<&str> = input.local_prefixes.iter().map(|s| s.as_str()).collect();
    let condition_refs: Vec<&str> = input.export_conditions.iter().map(|s| s.as_str()).collect();
    let mut output_chunks: HashMap<String, String> = HashMap::new();
    let mut chunk_source_maps: HashMap<String, SourceMap> = HashMap::new();
    let canon_cache: CanonCache = DashMap::new();

    // Process main chunk first to get specifier→namespace mapping
    let main_chunk = &chunk_graph.chunks[0];
    let subpath_ctx = Some(shake::SubpathImportContext {
        root_dir: &input.root_dir,
        export_conditions: &condition_refs,
    });
    let main_unused = if input.options.tree_shake {
        // Lazy chunks consume symbols from main cross-chunk; those consumptions
        // are invisible to the per-chunk shake analysis below. Precompute them
        // so the analyzer won't mark them unused and strip their declarations.
        let mut lazy_consumers: Vec<PathBuf> = Vec::new();
        for chunk in &chunk_graph.chunks[1..] {
            lazy_consumers.extend(chunk.modules.iter().cloned());
        }
        let externally_used = shake::collect_cross_chunk_used_names(
            &lazy_consumers,
            &main_chunk.modules,
            &input.modules,
            &prefix_refs,
            subpath_ctx,
        )?;

        shake::analyze_unused_exports(
            &main_chunk.modules,
            &input.modules,
            &main_chunk.entry,
            &prefix_refs,
            Some(&externally_used),
            subpath_ctx,
        )?
    } else {
        HashMap::new()
    };

    let main_module_set: HashSet<PathBuf> = main_chunk.modules.iter().cloned().collect();
    let main_result = bundle_chunk(&ChunkBundleParams {
        module_paths: &main_chunk.modules,
        all_modules: &input.modules,
        root_dir: &input.root_dir,
        prefix_refs: &prefix_refs,
        specifier_rewrites: &specifier_rewrites,
        per_module_maps: &input.per_module_maps,
        generate_source_maps: input.options.source_maps,
        unused_exports: &main_unused,
        bundled_specifiers: &input.bundled_specifiers,
        chunk_entry: &main_chunk.entry,
        chunk_kind: &main_chunk.kind,
        chunk_module_set: &main_module_set,
        main_file_to_ns: &HashMap::new(),
        main_chunk_module_set: &main_module_set,
        canon_cache: &canon_cache,
        export_conditions: &condition_refs,
    })?;
    let main_spec_to_ns = main_result.specifier_to_namespace;
    let main_file_to_ns = main_result.file_to_namespace;
    output_chunks.insert(main_chunk.filename.clone(), main_result.code);
    if let Some(map) = main_result.source_map {
        chunk_source_maps.insert(main_chunk.filename.clone(), map);
    }

    // Process lazy/shared chunks in parallel. Each chunk only reads shared
    // state (module map, rewrite map, main-chunk namespaces) so per-chunk
    // work is independent; we fan out with rayon and fold the results
    // afterwards to preserve the original aggregation semantics.
    let lazy_outputs: Vec<(String, ChunkBundleResult)> = chunk_graph.chunks[1..]
        .par_iter()
        .map(|chunk| -> NgcResult<(String, ChunkBundleResult)> {
            let unused_exports = if input.options.tree_shake {
                shake::analyze_unused_exports(
                    &chunk.modules,
                    &input.modules,
                    &chunk.entry,
                    &prefix_refs,
                    None,
                    subpath_ctx,
                )?
            } else {
                HashMap::new()
            };

            let lazy_module_set: HashSet<PathBuf> = chunk.modules.iter().cloned().collect();
            let result = bundle_chunk(&ChunkBundleParams {
                module_paths: &chunk.modules,
                all_modules: &input.modules,
                root_dir: &input.root_dir,
                prefix_refs: &prefix_refs,
                specifier_rewrites: &specifier_rewrites,
                per_module_maps: &input.per_module_maps,
                generate_source_maps: input.options.source_maps,
                unused_exports: &unused_exports,
                bundled_specifiers: &input.bundled_specifiers,
                chunk_entry: &chunk.entry,
                chunk_kind: &chunk.kind,
                chunk_module_set: &lazy_module_set,
                main_file_to_ns: &main_file_to_ns,
                main_chunk_module_set: &main_module_set,
                canon_cache: &canon_cache,
                export_conditions: &condition_refs,
            })?;
            Ok((chunk.filename.clone(), result))
        })
        .collect::<NgcResult<Vec<_>>>()?;

    let mut all_needed_npm: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut all_needed_project: BTreeSet<String> = BTreeSet::new();
    for (filename, result) in lazy_outputs {
        // Collect npm symbols this chunk needs.
        // Namespace imports ("* as X") are stored with the local name only.
        for ext in &result.npm_externals {
            let entry = all_needed_npm.entry(ext.source.clone()).or_default();
            for name in &ext.named_imports {
                if let Some(local) = name.strip_prefix("* as ") {
                    entry.insert(format!("__ns_import__{local}"));
                } else {
                    entry.insert(name.clone());
                }
            }
            if let Some(ref default) = ext.default_import {
                // Tag default imports so the export generator uses `ns.default`
                // instead of `ns.symbolName`.
                entry.insert(format!("__default__{default}"));
            }
        }

        // Collect project symbols this chunk needs from main
        all_needed_project.extend(result.project_cross_chunk_symbols);

        output_chunks.insert(filename.clone(), result.code);
        if let Some(map) = result.source_map {
            chunk_source_maps.insert(filename, map);
        }
    }

    // Generate cross-chunk export code (appended AFTER minification to avoid
    // the minifier stripping the `export { ... }` statement).
    let reexport_code = if !all_needed_npm.is_empty() || !all_needed_project.is_empty() {
        Some(generate_cross_chunk_exports(
            &all_needed_npm,
            &main_spec_to_ns,
            &all_needed_project,
        ))
    } else {
        None
    };

    // Minification pass — each chunk's parse + codegen is independent of
    // every other chunk, so fan out across rayon workers.
    if input.options.minify {
        let minified: Vec<(String, String, Option<SourceMap>)> = output_chunks
            .par_iter()
            .map(|(filename, code)| -> NgcResult<_> {
                let bundle_map = chunk_source_maps.get(filename);
                let result = minify::minify_chunk(code, filename, bundle_map)?;
                Ok((filename.clone(), result.code, result.source_map))
            })
            .collect::<NgcResult<Vec<_>>>()?;

        output_chunks = HashMap::with_capacity(minified.len());
        chunk_source_maps = HashMap::with_capacity(minified.len());
        for (filename, code, map) in minified {
            output_chunks.insert(filename.clone(), code);
            if let Some(map) = map {
                chunk_source_maps.insert(filename, map);
            }
        }
    }

    // Append cross-chunk exports after minification
    if let Some(reexport_code) = reexport_code {
        if let Some(main_code) = output_chunks.get_mut("main.js") {
            main_code.push_str(&reexport_code);
        }
    }

    // Content-hash filenames
    let main_filename = if input.options.content_hash {
        apply_content_hashes(&mut output_chunks, &mut chunk_source_maps)?
    } else {
        "main.js".to_string()
    };

    Ok(BundleOutput {
        chunks: output_chunks,
        main_filename,
        chunk_source_maps,
    })
}

/// Build a mapping from raw import specifiers (as they appear in source) to chunk filenames.
///
/// The chunk graph maps canonical `PathBuf` → chunk filename.
/// The rewriter needs raw specifier string → chunk filename.
/// We bridge this by computing relative specifiers from each importing module.
fn build_specifier_rewrite_map(
    input: &BundleInput,
    dynamic_import_map: &HashMap<PathBuf, String>,
) -> NgcResult<HashMap<String, String>> {
    if dynamic_import_map.is_empty() {
        return Ok(HashMap::new());
    }

    let mut rewrites: HashMap<String, String> = HashMap::new();

    // For each dynamic import target, compute the specifiers that could reference it.
    // We need to match what appears in the source code. The import scanner uses regex
    // to extract specifiers, so we need to match those exact strings.
    // Walk all modules and find dynamic import specifiers that resolve to chunk targets.
    for module_path in input.modules.keys() {
        let module_dir = module_path.parent();
        for (target_path, chunk_filename) in dynamic_import_map {
            if let Some(dir) = module_dir {
                // Compute relative path from module to target
                if let Ok(relative) = pathdiff(target_path, dir) {
                    // Try common specifier forms
                    let rel_str = relative.to_string_lossy();
                    let specifier = if rel_str.starts_with('.') {
                        rel_str.to_string()
                    } else {
                        format!("./{rel_str}")
                    };

                    // Strip known extensions to match how imports typically appear
                    for ext in &[".ts", ".tsx", ".js", ".mjs"] {
                        if let Some(stripped) = specifier.strip_suffix(ext) {
                            rewrites.insert(stripped.to_string(), chunk_filename.clone());
                        }
                    }
                    rewrites.insert(specifier, chunk_filename.clone());
                }
            }
        }
    }

    Ok(rewrites)
}

/// Compute a relative path from `base` directory to `target`.
fn pathdiff(target: &std::path::Path, base: &std::path::Path) -> Result<PathBuf, ()> {
    // Use a simple implementation: strip common prefix, add ../ for remaining base components
    let target_components: Vec<_> = target.components().collect();
    let base_components: Vec<_> = base.components().collect();

    let common_len = target_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    if common_len == 0 {
        return Err(());
    }

    let mut result = PathBuf::new();
    for _ in common_len..base_components.len() {
        result.push("..");
    }
    for component in &target_components[common_len..] {
        result.push(component);
    }

    Ok(result)
}

/// A module section ready for concatenation, with its source map and line count.
struct ModuleSection {
    /// The code section including the `// path` comment line.
    code: String,
    /// Number of lines in this section.
    line_count: u32,
    /// The canonical source path, for looking up the transform source map.
    source_path: PathBuf,
}

/// Parameters for bundling a single chunk.
struct ChunkBundleParams<'a> {
    module_paths: &'a [PathBuf],
    all_modules: &'a HashMap<PathBuf, String>,
    root_dir: &'a Path,
    prefix_refs: &'a [&'a str],
    specifier_rewrites: &'a HashMap<String, String>,
    per_module_maps: &'a HashMap<PathBuf, SourceMap>,
    generate_source_maps: bool,
    unused_exports: &'a HashMap<PathBuf, HashSet<String>>,
    bundled_specifiers: &'a HashSet<String>,
    /// The chunk's entry module — exports from this module are preserved.
    chunk_entry: &'a Path,
    /// The kind of chunk being bundled (Main, Lazy, or Shared).
    chunk_kind: &'a ChunkKind,
    /// Set of canonical module paths in this chunk (for cross-chunk import detection).
    chunk_module_set: &'a HashSet<PathBuf>,
    /// Main chunk's file path → namespace mapping (for resolving npm cross-chunk imports).
    main_file_to_ns: &'a HashMap<PathBuf, String>,
    /// Set of canonical module paths in the main chunk.
    main_chunk_module_set: &'a HashSet<PathBuf>,
    /// Shared cache of `canonicalize()` results across all per-chunk work.
    canon_cache: &'a CanonCache,
    /// Active `exports` conditions, forwarded to the npm resolver.
    export_conditions: &'a [&'a str],
}

/// Result of bundling a single chunk, including cross-chunk dependency info.
struct ChunkBundleResult {
    /// The bundled chunk code.
    code: String,
    /// Optional source map for the chunk.
    source_map: Option<SourceMap>,
    /// For lazy/shared chunks: npm external imports that need re-exporting from main.
    npm_externals: Vec<ExternalImport>,
    /// For lazy/shared chunks: project symbols imported from main chunk modules.
    project_cross_chunk_symbols: BTreeSet<String>,
    /// For main chunk: bare specifier → namespace variable name mapping.
    specifier_to_namespace: HashMap<String, String>,
    /// For main chunk: canonical file path → namespace variable name mapping.
    file_to_namespace: HashMap<PathBuf, String>,
}

/// Bundle a single chunk's modules into an ESM string, optionally with a source map.
fn bundle_chunk(p: &ChunkBundleParams<'_>) -> NgcResult<ChunkBundleResult> {
    let is_lazy = *p.chunk_kind != ChunkKind::Main;

    let mut all_externals: Vec<ExternalImport> = Vec::new();
    let mut sections: Vec<ModuleSection> = Vec::new();

    // Build namespace map: npm file path → namespace variable name
    // and specifier → namespace for project code imports.
    // Only needed for the main chunk — lazy chunks import npm symbols from main.
    let node_modules_dir = p.root_dir.join("node_modules");
    let mut file_to_namespace: HashMap<PathBuf, String> = HashMap::new();
    let mut specifier_to_namespace: HashMap<String, String> = HashMap::new();

    if !is_lazy {
        // First pass: assign namespaces to all npm modules in this chunk
        for module_path in p.module_paths {
            let is_npm = module_path
                .components()
                .any(|c| c.as_os_str() == "node_modules");
            if is_npm {
                let ns = crate::npm_wrap::namespace_from_path(module_path, &node_modules_dir);
                debug!(path = %module_path.display(), namespace = %ns, "assigned npm namespace");
                file_to_namespace.insert(module_path.clone(), ns);
            }
        }
        debug!(
            npm_module_count = file_to_namespace.len(),
            total_modules = p.module_paths.len(),
            "npm namespace assignment complete"
        );

        // Build specifier → namespace mapping for project code and npm cross-references.
        // Resolve each bare specifier to its actual entry file path and look up the namespace.
        for spec in p.bundled_specifiers.iter() {
            // Try to resolve the specifier to its entry file using the npm resolver
            if let Ok(entry_path) = ngc_npm_resolver::resolve::resolve_bare_specifier(
                spec,
                node_modules_dir.parent().unwrap_or(&node_modules_dir),
                p.export_conditions,
            ) {
                let canonical =
                    cached_canonicalize(p.canon_cache, &entry_path).unwrap_or(entry_path);
                if let Some(ns) = file_to_namespace.get(&canonical) {
                    specifier_to_namespace.insert(spec.clone(), ns.clone());
                    continue;
                }
            }
            // Fallback: for vendored/synthetic modules (e.g., @oxc-project/runtime/helpers/decorate)
            // that don't have a package.json, match by path suffix in the file_to_namespace map.
            let spec_path_suffix = spec.replace('/', std::path::MAIN_SEPARATOR_STR);
            for (path, ns) in &file_to_namespace {
                let path_str = path.to_string_lossy();
                if path_str.contains(&spec_path_suffix) {
                    specifier_to_namespace.insert(spec.clone(), ns.clone());
                    break;
                }
            }
        }
    }

    // Per-module work is independent: each module only reads immutable
    // pre-built maps (`file_to_namespace`, `specifier_to_namespace`, chunk
    // params) plus its own source code, and emits an optional ModuleSection
    // plus zero or more ExternalImports. Fan out across rayon workers and
    // merge the results back into `sections` / `all_externals` afterwards.
    let empty_specifiers: HashSet<String> = HashSet::new();
    let empty_ns_map: HashMap<String, String> = HashMap::new();
    let empty_prefixes: Vec<&str> = Vec::new();

    let per_module: Vec<(Option<ModuleSection>, Vec<ExternalImport>)> = p
        .module_paths
        .par_iter()
        .map(
            |module_path| -> NgcResult<(Option<ModuleSection>, Vec<ExternalImport>)> {
                let js_code =
                    p.all_modules
                        .get(module_path)
                        .ok_or_else(|| NgcError::BundleError {
                            message: format!(
                                "module {} is in the graph but has no transformed code",
                                module_path.display()
                            ),
                        })?;

                let is_npm = file_to_namespace.contains_key(module_path);
                let file_name = module_path.to_string_lossy();

                if is_npm {
                    // NPM module: wrap in IIFE with namespace isolation.
                    let namespace = &file_to_namespace[module_path];
                    let wrapped = crate::npm_wrap::wrap_npm_module(
                        js_code,
                        &file_name,
                        namespace,
                        |specifier| {
                            if specifier.starts_with('.') {
                                let from_dir = module_path.parent()?;
                                let target = from_dir.join(specifier);
                                for candidate in &[
                                    target.clone(),
                                    target.with_extension("mjs"),
                                    target.with_extension("js"),
                                    target.join("index.mjs"),
                                    target.join("index.js"),
                                ] {
                                    if let Some(canonical) =
                                        cached_canonicalize(p.canon_cache, candidate)
                                    {
                                        if let Some(ns) = file_to_namespace.get(&canonical) {
                                            return Some(ns.clone());
                                        }
                                    }
                                }
                                None
                            } else {
                                specifier_to_namespace.get(specifier).cloned()
                            }
                        },
                    )?;

                    let section = build_section(&wrapped.wrapped_code, module_path, p.root_dir);
                    Ok((section, Vec::new()))
                } else {
                    // Project module: use existing rewriter with namespace map.
                    // For lazy/shared chunks, pass empty prefixes/bundled so ALL
                    // imports are collected as external (for cross-chunk
                    // resolution).
                    let (effective_prefixes, effective_bundled, effective_ns_map) = if is_lazy {
                        (empty_prefixes.as_slice(), &empty_specifiers, &empty_ns_map)
                    } else {
                        (p.prefix_refs, p.bundled_specifiers, &specifier_to_namespace)
                    };

                    let module_unused = p.unused_exports.get(module_path);
                    let is_chunk_entry = module_path == p.chunk_entry;
                    let rewritten = rewrite::rewrite_module_with_shaking(
                        js_code,
                        &file_name,
                        effective_prefixes,
                        p.specifier_rewrites,
                        module_unused,
                        effective_bundled,
                        effective_ns_map,
                        is_chunk_entry,
                    )?;

                    let module_externals = if is_lazy {
                        classify_lazy_externals(rewritten.external_imports, module_path, p)
                    } else {
                        rewritten.external_imports
                    };

                    let section = build_section(&rewritten.code, module_path, p.root_dir);
                    Ok((section, module_externals))
                }
            },
        )
        .collect::<NgcResult<Vec<_>>>()?;

    for (section, externals) in per_module {
        if let Some(section) = section {
            sections.push(section);
        }
        all_externals.extend(externals);
    }

    // For lazy/shared chunks, all remaining externals are cross-chunk imports
    // (same-chunk imports were already filtered in the per-module loop above).
    // Merge them all into a single import from ./main.js.
    let mut npm_externals: Vec<ExternalImport> = Vec::new();
    let mut project_cross_chunk_symbols: BTreeSet<String> = BTreeSet::new();
    if is_lazy {
        let mut main_js_named = BTreeSet::new();
        let mut main_js_default = None;

        for ext in all_externals {
            // Classify: npm externals go through namespace lookup, project externals
            // are already top-level declarations in main.js.
            let is_from_npm = ext.source.starts_with("__resolved_ns__")
                || ext.source.starts_with("__npm_")
                || p.bundled_specifiers.contains(&ext.source)
                || (!ext.source.starts_with('.')
                    && !p.prefix_refs.iter().any(|pfx| ext.source.starts_with(pfx)));

            if is_from_npm {
                npm_externals.push(ext.clone());
            } else {
                // Project symbol — already in main chunk scope
                for name in &ext.named_imports {
                    let local = name.strip_prefix("* as ").unwrap_or(name);
                    project_cross_chunk_symbols.insert(local.to_string());
                }
                if let Some(ref default) = ext.default_import {
                    project_cross_chunk_symbols.insert(default.clone());
                }
            }

            // Collect all symbols for the ./main.js import
            for name in &ext.named_imports {
                if let Some(local) = name.strip_prefix("* as ") {
                    main_js_named.insert(local.to_string());
                } else {
                    main_js_named.insert(name.clone());
                }
            }
            if main_js_default.is_none() {
                main_js_default.clone_from(&ext.default_import);
            }
        }

        all_externals = Vec::new();

        // Merge all cross-chunk imports into a single import from ./main.js.
        // Convert default imports to named imports because main.js uses
        // `export { sym }` (named), not `export default`.
        if let Some(default_name) = main_js_default {
            main_js_named.insert(default_name);
        }
        if !main_js_named.is_empty() {
            all_externals.push(ExternalImport {
                source: "./main.js".to_string(),
                default_import: None,
                named_imports: main_js_named,
                is_side_effect: false,
            });
        }
    }

    let mut merged = merge_external_imports(all_externals);
    deduplicate_import_names(&mut merged);
    let mut output = String::new();

    // Write hoisted imports preamble
    for imp in &merged {
        output.push_str(&format_import(imp));
        output.push('\n');
    }

    // Track how many lines the preamble occupies
    let preamble_lines = if merged.is_empty() {
        0u32
    } else {
        // One line per import + one blank separator line
        merged.len() as u32 + 1
    };

    if !merged.is_empty() && !sections.is_empty() {
        output.push('\n');
    }

    // Build source map inputs: collect (source_map_ref, line_offset) pairs
    let mut sourcemap_entries: Vec<(SourceMap, u32)> = Vec::new();
    let mut current_line = preamble_lines;

    for (i, section) in sections.iter().enumerate() {
        // The comment line ("// src/path.js") is at current_line.
        // Module code starts at current_line + 1.
        let module_code_start = current_line + 1;

        if p.generate_source_maps {
            if let Some(transform_map) = p.per_module_maps.get(&section.source_path) {
                sourcemap_entries.push((transform_map.clone(), module_code_start));
            }
        }

        output.push_str(&section.code);
        if i < sections.len() - 1 {
            output.push_str("\n\n");
            current_line += section.line_count + 1; // section lines + blank separator
        } else {
            output.push('\n');
            current_line += section.line_count;
        }
    }

    // Build combined source map
    let combined_map = if p.generate_source_maps && !sourcemap_entries.is_empty() {
        let refs: Vec<(&SourceMap, u32)> = sourcemap_entries
            .iter()
            .map(|(map, offset)| (map, *offset))
            .collect();
        let builder = ConcatSourceMapBuilder::from_sourcemaps(&refs);
        Some(builder.into_sourcemap())
    } else {
        None
    };

    Ok(ChunkBundleResult {
        code: output,
        source_map: combined_map,
        npm_externals,
        project_cross_chunk_symbols,
        specifier_to_namespace,
        file_to_namespace,
    })
}

/// Build a `// path\n<code>` section from a rewritten module's code, or
/// `None` if the code was empty after trimming.
fn build_section(code: &str, module_path: &Path, root_dir: &Path) -> Option<ModuleSection> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return None;
    }
    let relative = module_path.strip_prefix(root_dir).unwrap_or(module_path);
    let display_path = relative.with_extension("js");
    let section_code = format!("// {}\n{}", display_path.display(), trimmed);
    let line_count = section_code.chars().filter(|&c| c == '\n').count() as u32 + 1;
    Some(ModuleSection {
        code: section_code,
        line_count,
        source_path: module_path.to_path_buf(),
    })
}

/// Classify a lazy/shared chunk module's external imports into the ones that
/// actually need to be hoisted as cross-chunk references (import from
/// `./main.js`) vs. the ones that can be dropped because they resolve to
/// another module in the same lazy chunk or a different lazy chunk.
fn classify_lazy_externals(
    externals: Vec<ExternalImport>,
    module_path: &Path,
    p: &ChunkBundleParams<'_>,
) -> Vec<ExternalImport> {
    let module_dir = module_path.parent();
    let is_npm_module = module_path
        .components()
        .any(|c| c.as_os_str() == "node_modules");

    let mut out: Vec<ExternalImport> = Vec::with_capacity(externals.len());
    for ext in externals {
        // Subpath imports (`#foo`) point at a file under the importing
        // package's `imports` map — typically a project file, occasionally
        // a bare specifier. Resolve to the actual target so chunk-membership
        // is checked against the real path, not the alias string. Without
        // this, every `#`-aliased project file would be misclassified as an
        // npm cross-chunk import (since the alias is in `bundled_specifiers`)
        // and emit a stale `import { X } from './main.js'` even when X is
        // already declared in the lazy chunk itself.
        if ext.source.starts_with('#') {
            let resolved = ngc_npm_resolver::resolve::resolve_subpath_import(
                &ext.source,
                Some(module_path),
                p.root_dir,
                p.export_conditions,
            )
            .ok()
            .and_then(|target| cached_canonicalize(p.canon_cache, &target));
            match resolved {
                Some(ref resolved_path) => {
                    if p.chunk_module_set.contains(resolved_path) {
                        continue; // same chunk — discard
                    }
                    if p.main_chunk_module_set.contains(resolved_path) {
                        out.push(ext); // verified in main
                    }
                    // else: another lazy/shared chunk — discard
                    continue;
                }
                None => {
                    // Couldn't resolve — fall through to the bundled-specifier
                    // path so we still emit a cross-chunk import rather than
                    // dropping the reference.
                }
            }
        }
        if p.bundled_specifiers.contains(&ext.source) {
            // npm bare specifier → cross-chunk from main
            out.push(ext);
        } else if ext.source.starts_with('.') {
            if is_npm_module {
                // Relative import from an npm module to another npm module.
                // Resolve to find the target's namespace in the main chunk.
                let mut resolved_ext = ext.clone();
                if let Some(dir) = module_dir {
                    let target = dir.join(&ext.source);
                    for c in &[
                        target.clone(),
                        target.with_extension("js"),
                        target.with_extension("mjs"),
                        target.join("index.js"),
                        target.join("index.mjs"),
                    ] {
                        if let Some(canon) = cached_canonicalize(p.canon_cache, c) {
                            if let Some(ns) = p.main_file_to_ns.get(&canon) {
                                resolved_ext.source = format!("__resolved_ns__{ns}");
                                break;
                            }
                        }
                    }
                }
                // If resolution failed, still mark as npm-sourced so it
                // doesn't get misclassified as a project symbol.
                if !resolved_ext.source.starts_with("__resolved_ns__") {
                    resolved_ext.source = "__npm_unresolved__".to_string();
                }
                out.push(resolved_ext);
                continue;
            }
            // Relative import from project module — resolve from this
            // module's directory.
            let resolved = module_dir.and_then(|dir| {
                let candidate = dir.join(&ext.source);
                let candidate_str = candidate.to_string_lossy().to_string();
                // Append extensions (not replace) to handle paths like
                // ./logto-auth.service where .service is NOT an extension.
                for suffix in &["", ".ts", ".js", ".mjs"] {
                    let full = PathBuf::from(format!("{candidate_str}{suffix}"));
                    if let Some(canon) = cached_canonicalize(p.canon_cache, &full) {
                        return Some(canon);
                    }
                }
                None
            });
            match resolved {
                Some(ref resolved_path) => {
                    if p.chunk_module_set.contains(resolved_path) {
                        continue; // Same chunk — discard
                    }
                    if p.main_chunk_module_set.contains(resolved_path) {
                        out.push(ext); // Verified in main
                    }
                    // else: in another lazy/shared chunk — discard
                }
                None => {
                    // Unresolved relative import — discard (can't verify main)
                }
            }
        } else if p.prefix_refs.iter().any(|pfx| ext.source.starts_with(pfx)) {
            // Path alias import — cross-chunk from main
            out.push(ext);
        } else {
            // Unknown bare specifier — treat as npm
            out.push(ext);
        }
    }
    out
}

/// Generate export statements on the main chunk for symbols that lazy chunks need.
///
/// For npm symbols, emits `var` declarations from IIFE namespaces.
/// For project symbols, they're already in scope as top-level declarations.
/// Both are exported via a single `export { ... }` statement.
fn generate_cross_chunk_exports(
    needed_npm: &HashMap<String, BTreeSet<String>>,
    specifier_to_ns: &HashMap<String, String>,
    needed_project: &BTreeSet<String>,
) -> String {
    let mut all_symbols: BTreeSet<String> = BTreeSet::new();
    let mut var_lines: Vec<String> = Vec::new();

    for (specifier, symbols) in needed_npm {
        // Look up namespace: first by specifier, then by resolved namespace key
        let resolved_ns_key = specifier.strip_prefix("__resolved_ns__").map(String::from);
        let ns = specifier_to_ns.get(specifier).or(resolved_ns_key.as_ref());
        // Skip symbols without a namespace mapping — they come from npm modules
        // in non-main chunks and can't be exported from main.js.
        let Some(ns) = ns else {
            continue;
        };
        for sym in symbols {
            if let Some(local) = sym.strip_prefix("__ns_import__") {
                if all_symbols.insert(local.to_string()) {
                    var_lines.push(format!("var {local} = {ns};"));
                }
            } else if let Some(local) = sym.strip_prefix("__default__") {
                // Default import: the binding name is `local` but the
                // namespace property is `default`.
                if all_symbols.insert(local.to_string()) {
                    var_lines.push(format!("var {local} = {ns}.default;"));
                }
            } else if all_symbols.insert(sym.clone()) {
                var_lines.push(format!("var {sym} = {ns}.{sym};"));
            }
        }
    }

    // Additional project symbols from the needed_project set.
    for sym in needed_project {
        all_symbols.insert(sym.clone());
    }

    if all_symbols.is_empty() {
        return String::new();
    }

    let mut code = String::from("\n// Cross-chunk exports for lazy-loaded chunks\n");
    for line in &var_lines {
        code.push_str(line);
        code.push('\n');
    }
    let syms: Vec<&str> = all_symbols.iter().map(|s| s.as_str()).collect();
    code.push_str(&format!("export {{ {} }};\n", syms.join(", ")));
    code
}

/// Compute a truncated SHA-256 content hash (8 hex characters).
fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let result = Sha256::digest(content.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        result[0], result[1], result[2], result[3]
    )
}

/// Apply content hashes to all chunk filenames.
///
/// Processes chunks in dependency order (leaf chunks first) so that when a chunk
/// references another via dynamic import, the referenced chunk's hashed name is
/// already known. Returns the hashed main filename.
fn apply_content_hashes(
    chunks: &mut HashMap<String, String>,
    source_maps: &mut HashMap<String, SourceMap>,
) -> NgcResult<String> {
    // Build rename map: process all chunks, computing hashes
    // First do non-main chunks (lazy/shared), then main
    let filenames: Vec<String> = chunks.keys().cloned().collect();
    let mut rename_map: HashMap<String, String> = HashMap::new();

    // Process non-main chunks first (they might be referenced by main)
    for filename in &filenames {
        if filename == "main.js" {
            continue;
        }
        let code = chunks.get(filename).ok_or_else(|| NgcError::BundleError {
            message: format!("chunk {filename} disappeared during hashing"),
        })?;
        let hash = content_hash(code);
        let hashed_name = insert_hash_in_filename(filename, &hash);
        rename_map.insert(filename.clone(), hashed_name);
    }

    // Replace chunk filename references in all chunks
    for code in chunks.values_mut() {
        for (old_name, new_name) in &rename_map {
            *code = code.replace(old_name, new_name);
        }
    }

    // Now hash main.js (after references have been updated)
    if let Some(main_code) = chunks.get("main.js") {
        let hash = content_hash(main_code);
        let hashed_main = insert_hash_in_filename("main.js", &hash);
        rename_map.insert("main.js".to_string(), hashed_main.clone());

        // Replace main.js references in lazy/shared chunks (they import from ./main.js)
        for (filename, code) in chunks.iter_mut() {
            if filename != "main.js" {
                *code = code.replace("./main.js", &format!("./{hashed_main}"));
            }
        }
    }

    // Apply renames to the chunks and source_maps HashMaps
    for (old_name, new_name) in &rename_map {
        if let Some(code) = chunks.remove(old_name) {
            chunks.insert(new_name.clone(), code);
        }
        if let Some(map) = source_maps.remove(old_name) {
            source_maps.insert(new_name.clone(), map);
        }
    }

    let main_filename = rename_map
        .get("main.js")
        .cloned()
        .unwrap_or_else(|| "main.js".to_string());

    debug!(main = %main_filename, "applied content hashes");
    Ok(main_filename)
}

/// Insert a content hash into a filename: `chunk-foo.js` → `chunk-foo.a1b2c3d4.js`.
fn insert_hash_in_filename(filename: &str, hash: &str) -> String {
    if let Some(stem) = filename.strip_suffix(".js") {
        format!("{stem}.{hash}.js")
    } else {
        format!("{filename}.{hash}")
    }
}

/// Merge external imports by source, combining named imports and deduplicating.
fn merge_external_imports(imports: Vec<ExternalImport>) -> Vec<MergedImport> {
    let mut by_source: HashMap<String, MergedImport> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for imp in imports {
        if let Some(existing) = by_source.get_mut(&imp.source) {
            existing.named_imports.extend(imp.named_imports);
            if existing.default_import.is_none() {
                existing.default_import = imp.default_import;
            }
            if !imp.is_side_effect {
                existing.is_side_effect = false;
            }
        } else {
            order.push(imp.source.clone());
            by_source.insert(
                imp.source.clone(),
                MergedImport {
                    source: imp.source,
                    default_import: imp.default_import,
                    named_imports: imp.named_imports,
                    is_side_effect: imp.is_side_effect,
                },
            );
        }
    }

    order
        .into_iter()
        .filter_map(|source| by_source.remove(&source))
        .collect()
}

/// Deduplicate named imports across all merged imports.
///
/// When the same name is imported from two different sources (e.g. `catchError`
/// from both `rxjs` and `rxjs/operators`), alias the duplicate to avoid
/// `SyntaxError: Cannot declare an imported binding name twice`.
fn deduplicate_import_names(imports: &mut [MergedImport]) {
    let mut seen: HashSet<String> = HashSet::new();
    for imp in imports.iter_mut() {
        if imp.is_side_effect {
            continue;
        }
        if let Some(ref default) = imp.default_import {
            seen.insert(default.clone());
        }
        let mut replacements: Vec<(String, String)> = Vec::new();
        for name in imp.named_imports.iter() {
            // Skip namespace imports like "* as foo"
            if name.starts_with("* as") {
                seen.insert(name.clone());
                continue;
            }
            if !seen.insert(name.clone()) {
                // Duplicate — create an alias
                let alias = format!("{}$1", name);
                replacements.push((name.clone(), alias));
            }
        }
        for (old, new) in replacements {
            imp.named_imports.remove(&old);
            imp.named_imports.insert(format!("{old} as {new}"));
        }
    }
}

/// Format a merged import as an ESM import statement.
fn format_import(imp: &MergedImport) -> String {
    if imp.is_side_effect {
        return format!("import '{}';", imp.source);
    }

    let mut parts: Vec<String> = Vec::new();

    if let Some(default) = &imp.default_import {
        parts.push(default.clone());
    }

    if !imp.named_imports.is_empty() {
        let names: Vec<&str> = imp.named_imports.iter().map(|s| s.as_str()).collect();
        parts.push(format!("{{ {} }}", names.join(", ")));
    }

    format!("import {} from '{}';", parts.join(", "), imp.source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ngc_project_resolver::ImportKind;
    use petgraph::graph::DiGraph;

    fn make_path(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Helper to get the main chunk code from a BundleOutput.
    fn main_chunk(output: &BundleOutput) -> &str {
        output
            .chunks
            .get(&output.main_filename)
            .expect("main chunk should exist")
    }

    #[test]
    fn test_two_module_bundle() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/src/leaf.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, leaf, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/src/leaf.ts"),
            "export const x = 42;\n".to_string(),
        );
        modules.insert(
            make_path("/root/src/main.ts"),
            "import { x } from './leaf';\nconsole.log(x);\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/src/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root/src"),
            options: BundleOptions::default(),
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        let result = main_chunk(&output);
        assert_eq!(output.chunks.len(), 1, "should produce single chunk");
        // leaf should appear before main
        let leaf_pos = result.find("const x = 42").expect("leaf code present");
        let main_pos = result.find("console.log(x)").expect("main code present");
        assert!(leaf_pos < main_pos, "leaf should come before main");
        assert!(!result.contains("import { x }"), "local import removed");
        assert!(!result.contains("export"), "export removed");
    }

    #[test]
    fn test_external_import_deduplication() {
        let mut graph = DiGraph::new();
        let a = graph.add_node(make_path("/root/a.ts"));
        let b = graph.add_node(make_path("/root/b.ts"));
        let entry = graph.add_node(make_path("/root/main.ts"));
        graph.add_edge(entry, a, ImportKind::Static);
        graph.add_edge(entry, b, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/a.ts"),
            "import { Component } from '@angular/core';\nexport const a = Component;\n".to_string(),
        );
        modules.insert(
            make_path("/root/b.ts"),
            "import { Injectable } from '@angular/core';\nexport const b = Injectable;\n"
                .to_string(),
        );
        modules.insert(
            make_path("/root/main.ts"),
            "import { a } from './a';\nimport { b } from './b';\nconsole.log(a, b);\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root"),
            options: BundleOptions::default(),
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        let result = main_chunk(&output);
        // Should have a single merged import from @angular/core
        let import_count = result.matches("@angular/core").count();
        assert_eq!(import_count, 1, "imports should be merged");
        assert!(result.contains("Component"));
        assert!(result.contains("Injectable"));
    }

    #[test]
    fn test_unreachable_module_excluded() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/leaf.ts"));
        let entry = graph.add_node(make_path("/root/main.ts"));
        let _orphan = graph.add_node(make_path("/root/orphan.ts"));
        graph.add_edge(entry, leaf, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/leaf.ts"),
            "export const x = 1;\n".to_string(),
        );
        modules.insert(
            make_path("/root/main.ts"),
            "import { x } from './leaf';\nconsole.log(x);\n".to_string(),
        );
        modules.insert(
            make_path("/root/orphan.ts"),
            "export const orphan = true;\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root"),
            options: BundleOptions::default(),
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        let result = main_chunk(&output);
        assert!(
            !result.contains("orphan"),
            "orphan module should be excluded"
        );
    }

    #[test]
    fn test_code_splitting_produces_multiple_chunks() {
        // main --static--> routes --dynamic--> lazy
        let mut graph = DiGraph::new();
        let lazy = graph.add_node(make_path("/root/lazy.ts"));
        let routes = graph.add_node(make_path("/root/routes.ts"));
        let entry = graph.add_node(make_path("/root/main.ts"));
        graph.add_edge(entry, routes, ImportKind::Static);
        graph.add_edge(routes, lazy, ImportKind::Dynamic);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/lazy.ts"),
            "export class LazyComponent {}\n".to_string(),
        );
        modules.insert(
            make_path("/root/routes.ts"),
            "export const routes = [{ loadComponent: () => import('./lazy').then(m => m.LazyComponent) }];\n".to_string(),
        );
        modules.insert(
            make_path("/root/main.ts"),
            "import { routes } from './routes';\nconsole.log(routes);\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root"),
            options: BundleOptions::default(),
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        // Should produce 2 chunks: main + lazy
        assert_eq!(output.chunks.len(), 2, "should produce 2 chunks");

        let main_code = main_chunk(&output);
        // Main should NOT contain the lazy module's class declaration
        assert!(
            !main_code.contains("class LazyComponent"),
            "main chunk should not contain lazy module's class"
        );
        // Main should contain routes
        assert!(
            main_code.contains("routes"),
            "main chunk should contain routes"
        );

        // Find the lazy chunk
        let lazy_chunk = output
            .chunks
            .iter()
            .find(|(k, _)| k.starts_with("chunk-"))
            .expect("should have a lazy chunk");
        assert!(
            lazy_chunk.1.contains("class LazyComponent"),
            "lazy chunk should contain LazyComponent class"
        );

        // Main should have rewritten the import specifier
        assert!(
            main_code.contains(lazy_chunk.0.as_str()),
            "main chunk should reference the lazy chunk filename"
        );
    }

    #[test]
    fn test_worker_bundling_emits_separate_chunk_and_rewrites_url() {
        // main --worker--> compute.worker
        // compute.worker --static--> worker-dep
        let mut graph = DiGraph::new();
        let worker_dep = graph.add_node(make_path("/root/worker-dep.ts"));
        let worker = graph.add_node(make_path("/root/compute.worker.ts"));
        let entry = graph.add_node(make_path("/root/main.ts"));
        graph.add_edge(entry, worker, ImportKind::Worker);
        graph.add_edge(worker, worker_dep, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/worker-dep.ts"),
            "export function heavy(x) { return x * 2; }\n".to_string(),
        );
        modules.insert(
            make_path("/root/compute.worker.ts"),
            "import { heavy } from './worker-dep';\nself.onmessage = (e) => self.postMessage(heavy(e.data));\n"
                .to_string(),
        );
        modules.insert(
            make_path("/root/main.ts"),
            "const w = new Worker(new URL('./compute.worker', import.meta.url), { type: 'module' });\nconsole.log(w);\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root"),
            options: BundleOptions::default(),
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");

        // Exactly one main + one worker chunk.
        assert_eq!(output.chunks.len(), 2, "should produce 2 chunks");
        let worker_entry = output
            .chunks
            .iter()
            .find(|(k, _)| k.starts_with("worker-"))
            .expect("should have a worker chunk");
        assert_eq!(worker_entry.0, "worker-compute.js");

        // The worker chunk must contain both the worker module and its nested
        // static dependency — the worker has its own dependency graph.
        assert!(worker_entry.1.contains("onmessage"));
        assert!(
            worker_entry.1.contains("function heavy"),
            "worker chunk should inline its nested static dependency"
        );

        let main_code = main_chunk(&output);
        // Main should NOT contain the worker body.
        assert!(
            !main_code.contains("onmessage"),
            "main chunk should not contain worker module's body"
        );
        // Main should have rewritten the URL specifier to the emitted filename.
        assert!(
            main_code.contains("'./worker-compute.js'"),
            "main chunk should reference the worker chunk filename. Got:\n{main_code}"
        );
        assert!(
            !main_code.contains("./compute.worker"),
            "main chunk should no longer reference the raw worker source path"
        );
        // The `new Worker(new URL(...))` shell must remain intact — only the
        // inner specifier was rewritten.
        assert!(main_code.contains("new Worker(new URL("));
        assert!(main_code.contains("import.meta.url"));
    }

    #[test]
    fn test_format_import_named() {
        let imp = MergedImport {
            source: "@angular/core".to_string(),
            default_import: None,
            named_imports: BTreeSet::from(["Component".to_string(), "Injectable".to_string()]),
            is_side_effect: false,
        };
        assert_eq!(
            format_import(&imp),
            "import { Component, Injectable } from '@angular/core';"
        );
    }

    #[test]
    fn test_format_import_default_and_named() {
        let imp = MergedImport {
            source: "foo".to_string(),
            default_import: Some("Foo".to_string()),
            named_imports: BTreeSet::from(["bar".to_string()]),
            is_side_effect: false,
        };
        assert_eq!(format_import(&imp), "import Foo, { bar } from 'foo';");
    }

    #[test]
    fn test_format_import_side_effect() {
        let imp = MergedImport {
            source: "zone.js".to_string(),
            default_import: None,
            named_imports: BTreeSet::new(),
            is_side_effect: true,
        };
        assert_eq!(format_import(&imp), "import 'zone.js';");
    }

    #[test]
    fn test_bundle_with_source_maps() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/src/leaf.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, leaf, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/src/leaf.ts"),
            "export const x = 42;\n".to_string(),
        );
        modules.insert(
            make_path("/root/src/main.ts"),
            "import { x } from './leaf';\nconsole.log(x);\n".to_string(),
        );

        // Create simple source maps for each module
        let leaf_map = SourceMap::new(
            None,
            vec![],
            None,
            vec!["leaf.ts".into()],
            vec![Some("export const x = 42;\n".into())],
            vec![oxc_sourcemap::Token::new(0, 0, 0, 0, Some(0), None)].into_boxed_slice(),
            None,
        );
        let main_map = SourceMap::new(
            None,
            vec![],
            None,
            vec!["main.ts".into()],
            vec![Some(
                "import { x } from './leaf';\nconsole.log(x);\n".into(),
            )],
            vec![oxc_sourcemap::Token::new(0, 0, 0, 0, Some(0), None)].into_boxed_slice(),
            None,
        );

        let mut per_module_maps = HashMap::new();
        per_module_maps.insert(make_path("/root/src/leaf.ts"), leaf_map);
        per_module_maps.insert(make_path("/root/src/main.ts"), main_map);

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/src/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root/src"),
            options: BundleOptions {
                source_maps: true,
                ..BundleOptions::default()
            },
            per_module_maps,
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        assert!(
            !output.chunk_source_maps.is_empty(),
            "should have source maps"
        );

        let main_map = output
            .chunk_source_maps
            .get("main.js")
            .expect("main chunk should have a source map");
        let sources: Vec<_> = main_map.get_sources().collect();
        assert!(
            sources.len() >= 2,
            "source map should reference both original files"
        );
        // Verify it serializes to valid JSON
        let json = main_map.to_json_string();
        assert!(json.contains("\"sources\""), "should have sources field");
        assert!(json.contains("\"mappings\""), "should have mappings field");
    }

    #[test]
    fn test_bundle_without_source_maps() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/src/leaf.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, leaf, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/src/leaf.ts"),
            "export const x = 42;\n".to_string(),
        );
        modules.insert(
            make_path("/root/src/main.ts"),
            "import { x } from './leaf';\nconsole.log(x);\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/src/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root/src"),
            options: BundleOptions::default(),
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        assert!(
            output.chunk_source_maps.is_empty(),
            "should not have source maps when disabled"
        );
    }

    #[test]
    fn test_content_hash_deterministic() {
        assert_eq!(content_hash("hello"), content_hash("hello"));
        assert_ne!(content_hash("hello"), content_hash("world"));
        assert_eq!(content_hash("hello").len(), 8);
    }

    #[test]
    fn test_insert_hash_in_filename() {
        assert_eq!(
            insert_hash_in_filename("main.js", "abcd1234"),
            "main.abcd1234.js"
        );
        assert_eq!(
            insert_hash_in_filename("chunk-admin.js", "deadbeef"),
            "chunk-admin.deadbeef.js"
        );
    }

    #[test]
    fn test_content_hash_bundle() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/src/leaf.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, leaf, ImportKind::Static);

        let mut modules = HashMap::new();
        modules.insert(
            make_path("/root/src/leaf.ts"),
            "export const x = 42;\n".to_string(),
        );
        modules.insert(
            make_path("/root/src/main.ts"),
            "import { x } from './leaf';\nconsole.log(x);\n".to_string(),
        );

        let input = BundleInput {
            modules,
            graph,
            entry: make_path("/root/src/main.ts"),
            local_prefixes: vec![".".to_string()],
            root_dir: make_path("/root/src"),
            options: BundleOptions {
                content_hash: true,
                ..BundleOptions::default()
            },
            per_module_maps: HashMap::new(),
            bundled_specifiers: HashSet::new(),
            export_conditions: Vec::new(),
        };

        let output = bundle(&input).expect("should bundle");
        // Main filename should contain a hash
        assert_ne!(output.main_filename, "main.js");
        assert!(output.main_filename.starts_with("main."));
        assert!(output.main_filename.ends_with(".js"));
        // Chunk should exist with the hashed name
        assert!(output.chunks.contains_key(&output.main_filename));
    }
}
