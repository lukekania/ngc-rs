use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use ngc_project_resolver::ImportKind;
use oxc_sourcemap::{ConcatSourceMapBuilder, SourceMap};
use petgraph::graph::DiGraph;
use tracing::debug;

use crate::chunk::build_chunk_graph;
use crate::minify;
use crate::rewrite::{self, ExternalImport};
use crate::shake;

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
    let mut output_chunks: HashMap<String, String> = HashMap::new();
    let mut chunk_source_maps: HashMap<String, SourceMap> = HashMap::new();

    for chunk in &chunk_graph.chunks {
        // Run tree shaking analysis for this chunk if enabled
        let unused_exports = if input.options.tree_shake {
            shake::analyze_unused_exports(
                &chunk.modules,
                &input.modules,
                &chunk.entry,
                &prefix_refs,
            )?
        } else {
            HashMap::new()
        };

        let (chunk_code, chunk_map) = bundle_chunk(&ChunkBundleParams {
            module_paths: &chunk.modules,
            all_modules: &input.modules,
            root_dir: &input.root_dir,
            prefix_refs: &prefix_refs,
            specifier_rewrites: &specifier_rewrites,
            per_module_maps: &input.per_module_maps,
            generate_source_maps: input.options.source_maps,
            unused_exports: &unused_exports,
            bundled_specifiers: &input.bundled_specifiers,
        })?;
        output_chunks.insert(chunk.filename.clone(), chunk_code);
        if let Some(map) = chunk_map {
            chunk_source_maps.insert(chunk.filename.clone(), map);
        }
    }

    // Minification pass
    if input.options.minify {
        let mut minified_chunks: HashMap<String, String> = HashMap::new();
        let mut minified_maps: HashMap<String, SourceMap> = HashMap::new();

        for (filename, code) in &output_chunks {
            let bundle_map = chunk_source_maps.get(filename);
            let minified = minify::minify_chunk(code, filename, bundle_map)?;
            minified_chunks.insert(filename.clone(), minified.code);
            if let Some(map) = minified.source_map {
                minified_maps.insert(filename.clone(), map);
            }
        }

        output_chunks = minified_chunks;
        chunk_source_maps = minified_maps;
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
}

/// Bundle a single chunk's modules into an ESM string, optionally with a source map.
fn bundle_chunk(p: &ChunkBundleParams<'_>) -> NgcResult<(String, Option<SourceMap>)> {
    let mut all_externals: Vec<ExternalImport> = Vec::new();
    let mut sections: Vec<ModuleSection> = Vec::new();

    for module_path in p.module_paths {
        let js_code = p
            .all_modules
            .get(module_path)
            .ok_or_else(|| NgcError::BundleError {
                message: format!(
                    "module {} is in the graph but has no transformed code",
                    module_path.display()
                ),
            })?;

        let file_name = module_path.to_string_lossy();
        let module_unused = p.unused_exports.get(module_path);

        // If ALL exports are unused and the module is in the unused map, skip it entirely
        let rewritten = rewrite::rewrite_module_with_shaking(
            js_code,
            &file_name,
            p.prefix_refs,
            p.specifier_rewrites,
            module_unused,
            p.bundled_specifiers,
        )?;

        all_externals.extend(rewritten.external_imports);

        let trimmed = rewritten.code.trim();
        if !trimmed.is_empty() {
            let relative = module_path.strip_prefix(p.root_dir).unwrap_or(module_path);
            let display_path = relative.with_extension("js");
            let section_code = format!("// {}\n{}", display_path.display(), trimmed);
            let line_count = section_code.chars().filter(|&c| c == '\n').count() as u32 + 1;
            sections.push(ModuleSection {
                code: section_code,
                line_count,
                source_path: module_path.clone(),
            });
        }
    }

    // Deduplicate top-level names across npm modules to prevent shadowing errors.
    // When two npm modules define the same top-level name (e.g. `function zip()`),
    // we rename the later one by appending a numeric suffix and update all
    // references within that module section.
    deduplicate_section_names(&mut sections);

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

    Ok((output, combined_map))
}

/// Deduplicate top-level declaration names across module sections.
///
/// When multiple modules define the same top-level name (e.g., two npm packages
/// both define `function zip()`), this renames later occurrences by appending
/// a numeric suffix (`zip` → `zip$2`) and updates all references within that
/// module section.
fn deduplicate_section_names(sections: &mut [ModuleSection]) {
    let mut seen_names: HashMap<String, usize> = HashMap::new();

    // Regex for top-level declarations: function, var, let, const, class
    let decl_re =
        regex::Regex::new(r"(?m)^(?:function|var|let|const|class)\s+([a-zA-Z_$][a-zA-Z0-9_$]*)")
            .expect("valid regex");

    for section in sections.iter_mut() {
        // Only check npm modules (node_modules paths) for collisions
        let is_npm = section
            .source_path
            .components()
            .any(|c| c.as_os_str() == "node_modules");
        if !is_npm {
            // Still register project-level names so they're tracked
            for cap in decl_re.captures_iter(&section.code) {
                let name = cap[1].to_string();
                seen_names.entry(name).or_insert(0);
            }
            continue;
        }

        // Collect names declared in this section
        let names_in_section: Vec<String> = decl_re
            .captures_iter(&section.code)
            .map(|cap| cap[1].to_string())
            .collect();

        // Find collisions and build rename map
        let mut renames: Vec<(String, String)> = Vec::new();
        for name in &names_in_section {
            let count = seen_names.entry(name.clone()).or_insert(0);
            *count += 1;
            if *count > 1 {
                let new_name = format!("{name}${}", count);
                renames.push((name.clone(), new_name));
            }
        }

        // Apply renames to this section's code (whole-word replacement)
        for (old_name, new_name) in &renames {
            tracing::debug!(old = old_name, new = new_name, path = %section.source_path.display(), "renaming duplicate declaration");
            // Use word boundary replacement to avoid partial matches
            let word_re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(old_name)))
                .expect("valid regex");
            let replacement = regex::NoExpand(new_name.as_str());
            section.code = word_re.replace_all(&section.code, replacement).to_string();
            section.line_count = section.code.chars().filter(|&c| c == '\n').count() as u32 + 1;
        }
    }
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
        rename_map.insert("main.js".to_string(), hashed_main);
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
