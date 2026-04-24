//! Node modules resolution for the ngc-rs bundler.
//!
//! Resolves bare import specifiers (e.g. `@angular/core`, `rxjs/operators`) to
//! their ESM entry points in `node_modules`, then recursively crawls all
//! transitive imports to discover every file that needs to be bundled.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use dashmap::DashMap;
use ngc_diagnostics::NgcResult;
use ngc_project_resolver::ImportKind;
use rayon::prelude::*;
use tracing::debug;

pub mod package_json;
pub mod resolve;
pub mod scanner;

/// The result of resolving all npm dependencies.
#[derive(Debug)]
pub struct NpmResolution {
    /// Map from absolute file path to JavaScript source code.
    pub modules: HashMap<PathBuf, String>,
    /// Dependency edges between npm files: (from, to, kind).
    pub edges: Vec<(PathBuf, PathBuf, ImportKind)>,
    /// All bare specifiers that were resolved (direct + transitive).
    /// Used to tell the bundler which imports to treat as "local".
    pub resolved_specifiers: HashSet<String>,
}

/// Resolve all npm dependencies for a set of bare import specifiers.
///
/// Starting from the specifiers collected from project code, resolves each to
/// its entry file via `package.json`, reads the source, scans for further
/// imports, and recursively resolves those too. Returns all discovered files,
/// edges, and the set of resolved specifiers.
///
/// `conditions` is forwarded to the `package.json` exports matcher: it
/// determines which branch of a conditional exports object is selected
/// (e.g. `browser` vs `node`, `production` vs `development`).
pub fn resolve_npm_dependencies(
    specifiers: &[String],
    project_root: &Path,
    conditions: &[&str],
) -> NgcResult<NpmResolution> {
    let node_modules = project_root.join("node_modules");
    if !node_modules.is_dir() {
        debug!("no node_modules directory found, skipping npm resolution");
        return Ok(NpmResolution {
            modules: HashMap::new(),
            edges: Vec::new(),
            resolved_specifiers: HashSet::new(),
        });
    }

    let mut modules: HashMap<PathBuf, String> = HashMap::new();
    let mut edges: Vec<(PathBuf, PathBuf, ImportKind)> = Vec::new();
    let mut resolved_specifiers: HashSet<String> = HashSet::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    // Shared canonicalize cache. On a typical real-world app, unique import
    // targets get probed ~3× — each import site canonicalises the same
    // target path independently. Every call is a `__getattrlist` / `stat`
    // syscall which the flame graph ranks as the single largest self-time
    // cost of the build. Caching results across rayon workers collapses the
    // duplicates into one syscall per unique path.
    let canon_cache: DashMap<PathBuf, PathBuf> = DashMap::new();
    let canonicalize_cached = |p: PathBuf| -> PathBuf {
        if let Some(hit) = canon_cache.get(&p) {
            return hit.clone();
        }
        let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
        canon_cache.insert(p, canonical.clone());
        canonical
    };

    // Phase 1: Resolve initial bare specifiers to entry files in parallel.
    // Each lookup hits node_modules/<pkg>/package.json plus a few is_file
    // probes — fully independent per specifier.
    let initial_entries: Vec<(String, PathBuf)> = specifiers
        .par_iter()
        .filter_map(|spec| {
            let outcome = if spec.starts_with('#') {
                resolve::resolve_subpath_import(spec, None, project_root, conditions)
            } else {
                resolve::resolve_bare_specifier(spec, project_root, conditions)
            };
            match outcome {
                Ok(entry_path) => {
                    let canonical = canonicalize_cached(entry_path);
                    Some((spec.clone(), canonical))
                }
                Err(e) => {
                    debug!(specifier = spec, error = %e, "skipping unresolvable npm package");
                    None
                }
            }
        })
        .collect();

    let mut frontier: Vec<PathBuf> = Vec::new();
    for (spec, entry) in initial_entries {
        resolved_specifiers.insert(spec);
        if visited.insert(entry.clone()) {
            frontier.push(entry);
        }
    }

    // One import discovered during a frontier parse, already resolved to a
    // canonicalised file on disk. Carries its specifier text (only for bare
    // imports — relative ones don't expose a specifier to the bundler).
    type ResolvedImport = (Option<String>, PathBuf, ImportKind);

    // Phase 2: frontier-level parallel BFS. Each level's per-file work —
    // read source, scan imports, resolve each — is embarrassingly parallel;
    // between levels we merge into shared state serially so `visited`,
    // `modules`, `edges`, and `resolved_specifiers` stay coherent and
    // deterministic. Level N+1 is the set of newly-discovered files from
    // level N.
    while !frontier.is_empty() {
        let per_file: Vec<(PathBuf, String, Vec<ResolvedImport>)> = frontier
            .par_iter()
            .filter_map(|file_path| {
                let source = match std::fs::read_to_string(file_path) {
                    Ok(s) => s,
                    Err(e) => {
                        debug!(path = %file_path.display(), error = %e, "skipping unreadable npm file");
                        return None;
                    }
                };

                let scanned = scanner::scan_npm_imports(&source);
                let mut resolved_imports: Vec<ResolvedImport> = Vec::with_capacity(scanned.len());

                for import in &scanned {
                    let kind = if import.is_dynamic {
                        ImportKind::Dynamic
                    } else {
                        ImportKind::Static
                    };

                    let (target, specifier_tag) = if import.specifier.starts_with('.') {
                        (
                            resolve::resolve_relative_import(&import.specifier, file_path).ok(),
                            None,
                        )
                    } else if import.specifier.starts_with('#') {
                        match resolve::resolve_subpath_import(
                            &import.specifier,
                            Some(file_path),
                            project_root,
                            conditions,
                        ) {
                            Ok(p) => (Some(p), Some(import.specifier.clone())),
                            Err(_) => {
                                debug!(
                                    specifier = import.specifier,
                                    from = %file_path.display(),
                                    "skipping unresolvable subpath import"
                                );
                                (None, None)
                            }
                        }
                    } else {
                        match resolve::resolve_bare_specifier(
                            &import.specifier,
                            project_root,
                            conditions,
                        ) {
                            Ok(p) => (Some(p), Some(import.specifier.clone())),
                            Err(_) => {
                                debug!(
                                    specifier = import.specifier,
                                    from = %file_path.display(),
                                    "skipping unresolvable transitive npm dependency"
                                );
                                (None, None)
                            }
                        }
                    };

                    if let Some(target_path) = target {
                        // Canonicalize to avoid duplicate entries from
                        // different relative paths (e.g.
                        // `../Subscription.js` vs
                        // `../observable/../Subscription.js`). Shared cache
                        // across workers means each target is stat'd once.
                        let canonical = canonicalize_cached(target_path);
                        resolved_imports.push((specifier_tag, canonical, kind));
                    }
                }

                Some((file_path.clone(), source, resolved_imports))
            })
            .collect();

        let mut next_frontier: Vec<PathBuf> = Vec::new();
        for (file_path, source, imports) in per_file {
            for (specifier_tag, target, kind) in imports {
                edges.push((file_path.clone(), target.clone(), kind));
                if let Some(spec) = specifier_tag {
                    resolved_specifiers.insert(spec);
                }
                if visited.insert(target.clone()) {
                    next_frontier.push(target);
                }
            }
            modules.insert(file_path, source);
        }
        frontier = next_frontier;
    }

    debug!(
        file_count = modules.len(),
        specifier_count = resolved_specifiers.len(),
        edge_count = edges.len(),
        "npm dependency resolution complete"
    );

    Ok(NpmResolution {
        modules,
        edges,
        resolved_specifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_json::{DEVELOPMENT_BROWSER_CONDITIONS, PRODUCTION_BROWSER_CONDITIONS};
    use std::fs;

    const DEV: &[&str] = DEVELOPMENT_BROWSER_CONDITIONS;

    fn setup_crawl_fixture(dir: &Path) {
        // Package "alpha" that imports from "beta" and has internal relative imports
        let alpha_dir = dir.join("node_modules/alpha");
        fs::create_dir_all(alpha_dir.join("dist")).unwrap();
        fs::write(
            alpha_dir.join("package.json"),
            r#"{ "module": "./dist/index.mjs" }"#,
        )
        .unwrap();
        fs::write(
            alpha_dir.join("dist/index.mjs"),
            "import { helper } from './utils.mjs';\nimport { beta } from 'beta';\nexport const alpha = helper + beta;\n",
        )
        .unwrap();
        fs::write(
            alpha_dir.join("dist/utils.mjs"),
            "export const helper = 42;\n",
        )
        .unwrap();

        // Package "beta" — simple, no further deps
        let beta_dir = dir.join("node_modules/beta");
        fs::create_dir_all(&beta_dir).unwrap();
        fs::write(
            beta_dir.join("package.json"),
            r#"{ "module": "./index.mjs" }"#,
        )
        .unwrap();
        fs::write(beta_dir.join("index.mjs"), "export const beta = 99;\n").unwrap();
    }

    #[test]
    fn test_crawl_resolves_all_files() {
        let dir = tempfile::tempdir().unwrap();
        setup_crawl_fixture(dir.path());

        let result = resolve_npm_dependencies(&["alpha".to_string()], dir.path(), DEV)
            .expect("should resolve");

        // Should have 3 files: alpha/dist/index.mjs, alpha/dist/utils.mjs, beta/index.mjs
        assert_eq!(result.modules.len(), 3, "should discover 3 npm files");
    }

    #[test]
    fn test_crawl_resolves_transitive_deps() {
        let dir = tempfile::tempdir().unwrap();
        setup_crawl_fixture(dir.path());

        let result = resolve_npm_dependencies(&["alpha".to_string()], dir.path(), DEV)
            .expect("should resolve");

        // "beta" should be in resolved_specifiers even though only "alpha" was requested
        assert!(
            result.resolved_specifiers.contains("beta"),
            "transitive dep 'beta' should be resolved"
        );
        assert!(
            result.resolved_specifiers.contains("alpha"),
            "direct dep 'alpha' should be resolved"
        );
    }

    #[test]
    fn test_crawl_records_edges() {
        let dir = tempfile::tempdir().unwrap();
        setup_crawl_fixture(dir.path());

        let result = resolve_npm_dependencies(&["alpha".to_string()], dir.path(), DEV)
            .expect("should resolve");

        // Should have edges: index->utils (relative), index->beta (bare)
        assert_eq!(result.edges.len(), 2, "should have 2 dependency edges");
    }

    #[test]
    fn test_crawl_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        setup_crawl_fixture(dir.path());

        // Request both alpha and beta — beta should not be crawled twice
        let result =
            resolve_npm_dependencies(&["alpha".to_string(), "beta".to_string()], dir.path(), DEV)
                .expect("should resolve");

        assert_eq!(result.modules.len(), 3, "should still have only 3 files");
    }

    #[test]
    fn test_crawl_no_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        // No node_modules directory
        let result = resolve_npm_dependencies(&["anything".to_string()], dir.path(), DEV)
            .expect("should succeed");

        assert!(result.modules.is_empty());
        assert!(result.resolved_specifiers.is_empty());
    }

    #[test]
    fn test_crawl_missing_package_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();

        let result = resolve_npm_dependencies(&["nonexistent".to_string()], dir.path(), DEV)
            .expect("should succeed with warning");

        assert!(result.modules.is_empty());
    }

    /// Integration test covering the end-to-end dev vs prod split: a package
    /// exposes different entry files for the `development` and `production`
    /// conditions, and each build mode pulls in its own transitive graph.
    #[test]
    fn test_crawl_dev_vs_prod_entry_split() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("node_modules/envsplit");
        fs::create_dir_all(pkg_dir.join("dev")).unwrap();
        fs::create_dir_all(pkg_dir.join("prod")).unwrap();
        fs::write(
            pkg_dir.join("package.json"),
            r#"{
              "exports": {
                ".": {
                  "browser": {
                    "development": "./dev/entry.mjs",
                    "production": "./prod/entry.mjs"
                  },
                  "default": "./dev/entry.mjs"
                }
              }
            }"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("dev/entry.mjs"),
            "export const mode = 'dev';\nexport const verbose = true;\n",
        )
        .unwrap();
        fs::write(
            pkg_dir.join("prod/entry.mjs"),
            "export const mode = 'prod';\n",
        )
        .unwrap();

        let prod = resolve_npm_dependencies(
            &["envsplit".to_string()],
            dir.path(),
            PRODUCTION_BROWSER_CONDITIONS,
        )
        .expect("prod resolution");
        let dev = resolve_npm_dependencies(
            &["envsplit".to_string()],
            dir.path(),
            DEVELOPMENT_BROWSER_CONDITIONS,
        )
        .expect("dev resolution");

        let prod_keys: Vec<String> = prod
            .modules
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let dev_keys: Vec<String> = dev
            .modules
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        assert!(
            prod_keys.iter().any(|k| k.ends_with("prod/entry.mjs")),
            "production build should include prod/entry.mjs, got {prod_keys:?}"
        );
        assert!(
            !prod_keys.iter().any(|k| k.ends_with("dev/entry.mjs")),
            "production build should NOT include dev/entry.mjs, got {prod_keys:?}"
        );
        assert!(
            dev_keys.iter().any(|k| k.ends_with("dev/entry.mjs")),
            "development build should include dev/entry.mjs, got {dev_keys:?}"
        );
        assert!(
            !dev_keys.iter().any(|k| k.ends_with("prod/entry.mjs")),
            "development build should NOT include prod/entry.mjs, got {dev_keys:?}"
        );
    }
}
