//! Node modules resolution for the ngc-rs bundler.
//!
//! Resolves bare import specifiers (e.g. `@angular/core`, `rxjs/operators`) to
//! their ESM entry points in `node_modules`, then recursively crawls all
//! transitive imports to discover every file that needs to be bundled.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use ngc_diagnostics::NgcResult;
use ngc_project_resolver::ImportKind;
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
pub fn resolve_npm_dependencies(
    specifiers: &[String],
    project_root: &Path,
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
    let mut queue: VecDeque<PathBuf> = VecDeque::new();

    // Map from bare specifier to its resolved entry file
    let mut specifier_to_entry: HashMap<String, PathBuf> = HashMap::new();

    // Phase 1: Resolve initial bare specifiers to entry files
    for spec in specifiers {
        match resolve::resolve_bare_specifier(spec, project_root) {
            Ok(entry_path) => {
                let canonical = entry_path.canonicalize().unwrap_or(entry_path);
                resolved_specifiers.insert(spec.clone());
                specifier_to_entry.insert(spec.clone(), canonical.clone());
                if visited.insert(canonical.clone()) {
                    queue.push_back(canonical);
                }
            }
            Err(e) => {
                debug!(specifier = spec, error = %e, "skipping unresolvable npm package");
            }
        }
    }

    // Phase 2: BFS crawl — read files, scan imports, resolve, repeat
    while let Some(file_path) = queue.pop_front() {
        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(e) => {
                debug!(path = %file_path.display(), error = %e, "skipping unreadable npm file");
                continue;
            }
        };

        let scanned = scanner::scan_npm_imports(&source);

        for import in &scanned {
            let kind = if import.is_dynamic {
                ImportKind::Dynamic
            } else {
                ImportKind::Static
            };

            let resolved = if import.specifier.starts_with('.') {
                // Relative import within the package
                resolve::resolve_relative_import(&import.specifier, &file_path).ok()
            } else {
                // Bare specifier — transitive npm dependency
                match resolve::resolve_bare_specifier(&import.specifier, project_root) {
                    Ok(path) => {
                        resolved_specifiers.insert(import.specifier.clone());
                        specifier_to_entry.insert(import.specifier.clone(), path.clone());
                        Some(path)
                    }
                    Err(_) => {
                        debug!(
                            specifier = import.specifier,
                            from = %file_path.display(),
                            "skipping unresolvable transitive npm dependency"
                        );
                        None
                    }
                }
            };

            if let Some(target_path) = resolved {
                // Canonicalize to avoid duplicate entries from different relative paths
                // (e.g. "../Subscription.js" vs "../observable/../Subscription.js")
                let canonical = target_path.canonicalize().unwrap_or(target_path);
                edges.push((file_path.clone(), canonical.clone(), kind));
                if visited.insert(canonical.clone()) {
                    queue.push_back(canonical);
                }
            }
        }

        modules.insert(file_path.clone(), source);
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
    use std::fs;

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

        let result =
            resolve_npm_dependencies(&["alpha".to_string()], dir.path()).expect("should resolve");

        // Should have 3 files: alpha/dist/index.mjs, alpha/dist/utils.mjs, beta/index.mjs
        assert_eq!(result.modules.len(), 3, "should discover 3 npm files");
    }

    #[test]
    fn test_crawl_resolves_transitive_deps() {
        let dir = tempfile::tempdir().unwrap();
        setup_crawl_fixture(dir.path());

        let result =
            resolve_npm_dependencies(&["alpha".to_string()], dir.path()).expect("should resolve");

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

        let result =
            resolve_npm_dependencies(&["alpha".to_string()], dir.path()).expect("should resolve");

        // Should have edges: index->utils (relative), index->beta (bare)
        assert_eq!(result.edges.len(), 2, "should have 2 dependency edges");
    }

    #[test]
    fn test_crawl_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        setup_crawl_fixture(dir.path());

        // Request both alpha and beta — beta should not be crawled twice
        let result =
            resolve_npm_dependencies(&["alpha".to_string(), "beta".to_string()], dir.path())
                .expect("should resolve");

        assert_eq!(result.modules.len(), 3, "should still have only 3 files");
    }

    #[test]
    fn test_crawl_no_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        // No node_modules directory
        let result = resolve_npm_dependencies(&["anything".to_string()], dir.path())
            .expect("should succeed");

        assert!(result.modules.is_empty());
        assert!(result.resolved_specifiers.is_empty());
    }

    #[test]
    fn test_crawl_missing_package_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();

        let result = resolve_npm_dependencies(&["nonexistent".to_string()], dir.path())
            .expect("should succeed with warning");

        assert!(result.modules.is_empty());
    }
}
