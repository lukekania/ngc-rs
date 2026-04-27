//! Polyfills bundling pipeline.
//!
//! Resolves each entry in `angular.json`'s `polyfills[]` array — bare
//! specifiers via the npm resolver, relative paths via `ts-transform` — into a
//! synthetic entry module of side-effect imports, then runs that synthetic
//! entry through the same `ngc_bundler::bundle` pipeline `main.ts` uses. The
//! result is written as `dist/polyfills[.<hash>].js`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ngc_bundler::{BundleInput, BundleOptions};
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_npm_resolver::resolve::resolve_bare_specifier;
use ngc_npm_resolver::scanner::scan_npm_imports;
use ngc_project_resolver::ImportKind;
use petgraph::graph::DiGraph;

/// Result of generating the polyfills bundle.
pub struct PolyfillsBundle {
    /// Files written to disk (the polyfills JS, plus optional .map).
    pub output_files: Vec<PathBuf>,
    /// The polyfills filename to inject into `index.html` (e.g.
    /// `"polyfills.js"` or `"polyfills.<hash>.js"`).
    pub filename: String,
}

/// Resolve, transform, and bundle the polyfill entries listed in
/// `angular.json` into a single chunk written to `<out_dir>/polyfills.js`
/// (or `polyfills.<hash>.js` under `--configuration production`).
///
/// Each entry is treated as a side-effect module:
/// - Bare specifiers (e.g. `@angular/localize/init`) are resolved via the npm
///   resolver against `project_root`, then transitively crawled.
/// - Relative paths (e.g. `src/polyfills.ts`) are resolved against
///   `project_root`, transformed through `ts-transform`, and their imports
///   recursively followed (relative or bare).
pub fn generate_polyfills(
    polyfills: &[String],
    out_dir: &Path,
    project_root: &Path,
    bundle_options: BundleOptions,
    configuration: Option<&str>,
) -> NgcResult<PolyfillsBundle> {
    let export_conditions =
        ngc_npm_resolver::package_json::conditions_for_configuration(configuration);

    // Phase 1: classify each polyfill entry as bare or relative, and resolve
    // it to a canonical file path so we can build the synthetic entry's
    // import list and seed the dependency graph.
    let mut bare_specifiers: Vec<String> = Vec::new();
    let mut relative_seeds: Vec<PathBuf> = Vec::new();
    let mut entry_imports: Vec<EntryImport> = Vec::with_capacity(polyfills.len());

    for entry in polyfills {
        // Try project-relative resolution first (handles `"./src/polyfills.ts"`,
        // `"src/polyfills.ts"`); fall back to bare-specifier resolution against
        // node_modules. The order matters: a string like `"src/polyfills.ts"`
        // is syntactically a bare specifier but Angular treats it as
        // project-relative when the file exists on disk.
        if let Some(resolved) = try_resolve_project_relative(entry, project_root) {
            relative_seeds.push(resolved.clone());
            entry_imports.push(EntryImport::Relative(resolved));
        } else {
            let resolved = resolve_bare_specifier(entry, project_root, export_conditions)
                .map_err(|e| NgcError::BundleError {
                    message: format!("polyfill '{entry}' could not be resolved: {e}"),
                })?
                .canonicalize()
                .map_err(|e| NgcError::Io {
                    path: project_root.to_path_buf(),
                    source: e,
                })?;
            bare_specifiers.push(entry.clone());
            entry_imports.push(EntryImport::Bare {
                specifier: entry.clone(),
                resolved,
            });
        }
    }

    // Phase 2: crawl npm transitive dependencies for the bare entries.
    let npm_resolution = ngc_npm_resolver::resolve_npm_dependencies(
        &bare_specifiers,
        project_root,
        export_conditions,
    )?;

    // Phase 3: walk the relative-TS seeds, transforming each and resolving
    // their imports (relative → recurse; bare → fold into npm resolution).
    let mut modules: HashMap<PathBuf, String> = HashMap::new();
    let mut per_module_maps: HashMap<PathBuf, oxc_sourcemap::SourceMap> = HashMap::new();
    let mut local_edges: Vec<(PathBuf, PathBuf, ImportKind)> = Vec::new();
    let mut deferred_bare: Vec<String> = Vec::new();

    let mut visited_local: HashSet<PathBuf> = HashSet::new();
    let mut frontier = relative_seeds.clone();
    while let Some(file) = frontier.pop() {
        if !visited_local.insert(file.clone()) {
            continue;
        }
        let source = std::fs::read_to_string(&file).map_err(|e| NgcError::Io {
            path: file.clone(),
            source: e,
        })?;
        let (code, map) = ngc_ts_transform::transform_source_with_map(
            &source,
            &file.to_string_lossy(),
            bundle_options.source_maps,
        )?;
        for import in scan_npm_imports(&code) {
            if import.specifier.starts_with('.') {
                if let Some(target) = resolve_relative_ts(&import.specifier, &file) {
                    let kind = if import.is_dynamic {
                        ImportKind::Dynamic
                    } else {
                        ImportKind::Static
                    };
                    local_edges.push((file.clone(), target.clone(), kind));
                    if !visited_local.contains(&target) {
                        frontier.push(target);
                    }
                }
            } else if !import.specifier.starts_with('#')
                && !npm_resolution
                    .resolved_specifiers
                    .contains(&import.specifier)
                && !deferred_bare.contains(&import.specifier)
            {
                deferred_bare.push(import.specifier);
            }
        }
        modules.insert(file.clone(), code);
        if let Some(m) = map {
            per_module_maps.insert(file, m);
        }
    }

    // Resolve any bare specifiers that surfaced from the relative-TS seeds
    // but weren't part of the bare entry list.
    let extra_npm = if deferred_bare.is_empty() {
        None
    } else {
        Some(ngc_npm_resolver::resolve_npm_dependencies(
            &deferred_bare,
            project_root,
            export_conditions,
        )?)
    };

    // Phase 4: assemble modules + graph.
    let synthetic_entry = synthetic_entry_path(project_root);
    let synthetic_source = build_synthetic_entry(&entry_imports, project_root);
    modules.insert(synthetic_entry.clone(), synthetic_source);
    for (path, source) in &npm_resolution.modules {
        modules
            .entry(path.clone())
            .or_insert_with(|| source.clone());
    }
    if let Some(ref extra) = extra_npm {
        for (path, source) in &extra.modules {
            modules
                .entry(path.clone())
                .or_insert_with(|| source.clone());
        }
    }

    let mut graph: DiGraph<PathBuf, ImportKind> = DiGraph::new();
    let mut path_index: HashMap<PathBuf, _> = HashMap::new();
    for path in modules.keys() {
        let idx = graph.add_node(path.clone());
        path_index.insert(path.clone(), idx);
    }

    let synth_idx = path_index[&synthetic_entry];
    let entry_targets: Vec<&PathBuf> = entry_imports
        .iter()
        .map(|e| match e {
            EntryImport::Bare { resolved, .. } => resolved,
            EntryImport::Relative(p) => p,
        })
        .collect();
    for target in &entry_targets {
        if let Some(&to_idx) = path_index.get(*target) {
            graph.add_edge(synth_idx, to_idx, ImportKind::Static);
        }
    }
    // Chain consecutive polyfill entries: edge `entries[i+1] → entries[i]`
    // so the topo sort emits the earlier-declared entry first. Without this
    // pinning, sibling polyfills (no transitive dep between them) would be
    // emitted in the bundler's deterministic-but-arbitrary path order, not
    // the order the user wrote in `angular.json`.
    for window in entry_targets.windows(2) {
        let (prev, next) = (window[0], window[1]);
        if prev == next {
            continue;
        }
        if let (Some(&pi), Some(&ni)) = (path_index.get(prev), path_index.get(next)) {
            graph.add_edge(ni, pi, ImportKind::Static);
        }
    }
    for (from, to, kind) in &npm_resolution.edges {
        if let (Some(&fi), Some(&ti)) = (path_index.get(from), path_index.get(to)) {
            graph.add_edge(fi, ti, *kind);
        }
    }
    if let Some(ref extra) = extra_npm {
        for (from, to, kind) in &extra.edges {
            if let (Some(&fi), Some(&ti)) = (path_index.get(from), path_index.get(to)) {
                graph.add_edge(fi, ti, *kind);
            }
        }
    }
    for (from, to, kind) in &local_edges {
        if let (Some(&fi), Some(&ti)) = (path_index.get(from), path_index.get(to)) {
            graph.add_edge(fi, ti, *kind);
        }
    }

    let mut bundled_specifiers: HashSet<String> = npm_resolution.resolved_specifiers;
    if let Some(extra) = extra_npm {
        for s in extra.resolved_specifiers {
            bundled_specifiers.insert(s);
        }
    }

    // Phase 5: bundle. Disable content-hashing inside the bundler — the
    // bundler's hash machinery hard-codes `main.js` as the entry filename;
    // we apply hashing ourselves below so the output is named `polyfills`.
    // Disable dev-mode globals — they're a `main.js`-only prologue.
    let mut polyfill_bundle_options = bundle_options;
    polyfill_bundle_options.content_hash = false;
    polyfill_bundle_options.inject_dev_mode_globals = false;

    let bundle_input = BundleInput {
        modules,
        graph,
        entry: synthetic_entry,
        local_prefixes: vec![".".to_string()],
        root_dir: project_root.to_path_buf(),
        options: polyfill_bundle_options,
        per_module_maps,
        bundled_specifiers,
        export_conditions: export_conditions.iter().map(|s| (*s).to_string()).collect(),
    };

    let bundle_output = ngc_bundler::bundle(&bundle_input)?;

    // Phase 6: write output. The bundler emits the entry chunk as `main.js`;
    // rename to `polyfills[.hash].js` (and update any source-map URL).
    let main_code = bundle_output
        .chunks
        .get(&bundle_output.main_filename)
        .ok_or_else(|| NgcError::BundleError {
            message: "polyfills bundle produced no main chunk".to_string(),
        })?;

    let (filename, code) = if bundle_options.content_hash {
        let hash = content_hash(main_code);
        (format!("polyfills.{hash}.js"), main_code.clone())
    } else {
        ("polyfills.js".to_string(), main_code.clone())
    };

    let mut output_files = Vec::new();
    let mut final_code = code;
    if bundle_options.source_maps {
        if let Some(map) = bundle_output
            .chunk_source_maps
            .get(&bundle_output.main_filename)
        {
            if configuration == Some("production") {
                let map_filename = format!("{filename}.map");
                final_code.push_str(&format!("//# sourceMappingURL={map_filename}\n"));
                let map_path = out_dir.join(&map_filename);
                std::fs::write(&map_path, map.to_json_string()).map_err(|e| NgcError::Io {
                    path: map_path.clone(),
                    source: e,
                })?;
                output_files.push(map_path);
            } else {
                final_code.push_str(&format!("//# sourceMappingURL={}\n", map.to_data_url()));
            }
        }
    }

    let path = out_dir.join(&filename);
    std::fs::write(&path, &final_code).map_err(|e| NgcError::Io {
        path: path.clone(),
        source: e,
    })?;
    output_files.insert(0, path);

    Ok(PolyfillsBundle {
        output_files,
        filename,
    })
}

/// Resolve a relative import (e.g. `'./zone-shim'`) from a project-local TS
/// file. Probes `.ts`/`.tsx`/`.js`/`.mjs` extensions and `index.*` directory
/// fallbacks — covers the surface ts-transform's source files actually use.
/// The shared `npm_resolver::resolve_relative_import` only probes the
/// JS-side extensions so it can't follow a relative import into a `.ts`
/// sibling.
fn resolve_relative_ts(specifier: &str, from_file: &Path) -> Option<PathBuf> {
    let from_dir = from_file.parent()?;
    let base = from_dir.join(specifier);
    if base.is_file() {
        return base.canonicalize().ok();
    }
    for ext in &["ts", "tsx", "mjs", "js", "cjs"] {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }
    for index in &["index.ts", "index.tsx", "index.mjs", "index.js"] {
        let candidate = base.join(index);
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }
    None
}

/// Try to resolve a polyfill entry as a project-relative path. Probes the
/// literal path plus the common TS/JS extensions to mirror Angular's
/// resolver. Returns `None` for entries that aren't project-local files —
/// `generate_polyfills` then falls back to bare-specifier resolution.
fn try_resolve_project_relative(entry: &str, project_root: &Path) -> Option<PathBuf> {
    let trimmed = entry.trim_start_matches("./");
    let candidate = project_root.join(trimmed);
    let exts = ["", "ts", "tsx", "js", "mjs"];
    for ext in &exts {
        let probe = if ext.is_empty() {
            candidate.clone()
        } else if candidate.extension().is_some() {
            // Already has an extension — only probe the literal path.
            continue;
        } else {
            candidate.with_extension(ext)
        };
        if probe.is_file() {
            return probe.canonicalize().ok();
        }
    }
    None
}

/// One classified polyfill entry. Carries the canonical path of the resolved
/// file plus the original specifier text (for bare entries — used to emit the
/// import line in the synthetic entry).
enum EntryImport {
    Bare {
        specifier: String,
        resolved: PathBuf,
    },
    Relative(PathBuf),
}

/// Path used for the synthetic entry module. Lives at the project root so its
/// `./`-prefixed imports of relative-TS polyfills resolve cleanly.
fn synthetic_entry_path(project_root: &Path) -> PathBuf {
    project_root.join("__ngc_polyfills_entry__.js")
}

/// Build the synthetic entry source: one side-effect import per polyfill
/// entry, in declared order. Bare entries import their original specifier;
/// relative entries import a path computed from the project root.
fn build_synthetic_entry(entries: &[EntryImport], project_root: &Path) -> String {
    let mut out = String::new();
    for entry in entries {
        match entry {
            EntryImport::Bare { specifier, .. } => {
                out.push_str(&format!("import '{specifier}';\n"));
            }
            EntryImport::Relative(target) => {
                let rel = target.strip_prefix(project_root).unwrap_or(target);
                let rel_str = rel.to_string_lossy();
                let spec = if rel_str.starts_with('.') {
                    rel_str.to_string()
                } else {
                    format!("./{rel_str}")
                };
                out.push_str(&format!("import '{spec}';\n"));
            }
        }
    }
    out
}

/// SHA-256 over the chunk bytes, hex-encoded, truncated to 8 chars (matches
/// `ng build`'s default hash length).
fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let bytes = hasher.finalize();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_pkg(dir: &Path, name: &str, files: &[(&str, &str)]) {
        let pkg_dir = dir.join("node_modules").join(name);
        fs::create_dir_all(&pkg_dir).unwrap();
        for (rel, content) in files {
            let p = pkg_dir.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, content).unwrap();
        }
    }

    #[test]
    fn bare_specifier_polyfill_is_resolved_and_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().canonicalize().unwrap();
        let out_dir = project_root.join("dist");
        fs::create_dir_all(&out_dir).unwrap();

        // Mock @angular/localize/init: a real-looking polyfill body.
        write_pkg(
            &project_root,
            "@angular/localize",
            &[
                (
                    "package.json",
                    r#"{"name":"@angular/localize","exports":{"./init":"./init/index.mjs"}}"#,
                ),
                (
                    "init/index.mjs",
                    "globalThis.$localize = globalThis.$localize || function (parts) { return parts[0]; };\n",
                ),
            ],
        );

        let result = generate_polyfills(
            &["@angular/localize/init".to_string()],
            &out_dir,
            &project_root,
            BundleOptions::default(),
            None,
        )
        .unwrap();

        let polyfill_js = fs::read_to_string(out_dir.join(&result.filename)).unwrap();
        assert_eq!(result.filename, "polyfills.js");
        assert!(
            !polyfill_js.contains("import '@angular/localize/init';"),
            "bare specifier was not resolved:\n{polyfill_js}"
        );
        assert!(
            polyfill_js.contains("globalThis.$localize"),
            "polyfill runtime body missing from output:\n{polyfill_js}"
        );
    }

    #[test]
    fn relative_ts_polyfill_bundles_transitive_imports() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().canonicalize().unwrap();
        let out_dir = project_root.join("dist");
        fs::create_dir_all(&out_dir).unwrap();
        let src = project_root.join("src");
        fs::create_dir_all(&src).unwrap();

        fs::write(
            src.join("polyfills.ts"),
            "import './zone-shim';\nconsole.log('polyfills loaded');\n",
        )
        .unwrap();
        fs::write(
            src.join("zone-shim.ts"),
            "globalThis.__zoneShimLoaded = true;\n",
        )
        .unwrap();

        let result = generate_polyfills(
            &["src/polyfills.ts".to_string()],
            &out_dir,
            &project_root,
            BundleOptions::default(),
            None,
        )
        .unwrap();

        let polyfill_js = fs::read_to_string(out_dir.join(&result.filename)).unwrap();
        assert!(
            polyfill_js.contains("__zoneShimLoaded"),
            "transitive import body missing:\n{polyfill_js}"
        );
        assert!(
            polyfill_js.contains("polyfills loaded"),
            "entry body missing:\n{polyfill_js}"
        );
    }

    #[test]
    fn multiple_polyfills_bundle_in_declared_order_with_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().canonicalize().unwrap();
        let out_dir = project_root.join("dist");
        fs::create_dir_all(&out_dir).unwrap();

        // Two polyfill packages that both import a third (shared) one.
        write_pkg(
            &project_root,
            "alpha",
            &[
                ("package.json", r#"{"name":"alpha","module":"./index.mjs"}"#),
                ("index.mjs", "import 'shared';\nglobalThis.__alpha = 'A';\n"),
            ],
        );
        write_pkg(
            &project_root,
            "beta",
            &[
                ("package.json", r#"{"name":"beta","module":"./index.mjs"}"#),
                ("index.mjs", "import 'shared';\nglobalThis.__beta = 'B';\n"),
            ],
        );
        write_pkg(
            &project_root,
            "shared",
            &[
                (
                    "package.json",
                    r#"{"name":"shared","module":"./index.mjs"}"#,
                ),
                ("index.mjs", "globalThis.__shared = 'S';\n"),
            ],
        );

        let result = generate_polyfills(
            &["alpha".to_string(), "beta".to_string()],
            &out_dir,
            &project_root,
            BundleOptions::default(),
            None,
        )
        .unwrap();

        let polyfill_js = fs::read_to_string(out_dir.join(&result.filename)).unwrap();
        // Shared dep appears exactly once.
        assert_eq!(
            polyfill_js.matches("__shared = 'S'").count(),
            1,
            "shared dep was not deduped:\n{polyfill_js}"
        );
        // Both entries are present.
        assert!(polyfill_js.contains("__alpha = 'A'"));
        assert!(polyfill_js.contains("__beta = 'B'"));
        // Declared order: alpha before beta.
        let alpha_pos = polyfill_js.find("__alpha = 'A'").unwrap();
        let beta_pos = polyfill_js.find("__beta = 'B'").unwrap();
        assert!(
            alpha_pos < beta_pos,
            "declared order not preserved:\n{polyfill_js}"
        );
    }

    #[test]
    fn content_hash_flag_propagates_to_polyfills_filename() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().canonicalize().unwrap();
        let out_dir = project_root.join("dist");
        fs::create_dir_all(&out_dir).unwrap();
        write_pkg(
            &project_root,
            "@angular/localize",
            &[
                (
                    "package.json",
                    r#"{"name":"@angular/localize","exports":{"./init":"./init/index.mjs"}}"#,
                ),
                ("init/index.mjs", "globalThis.$localize = function () {};\n"),
            ],
        );
        let opts = BundleOptions {
            content_hash: true,
            ..BundleOptions::default()
        };
        let result = generate_polyfills(
            &["@angular/localize/init".to_string()],
            &out_dir,
            &project_root,
            opts,
            None,
        )
        .unwrap();
        assert!(result.filename.starts_with("polyfills."));
        assert!(result.filename.ends_with(".js"));
        assert_ne!(result.filename, "polyfills.js");
    }
}
