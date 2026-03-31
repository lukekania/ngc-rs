use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use petgraph::graph::{DiGraph, NodeIndex};
use rayon::prelude::*;
use tracing::debug;

use crate::import_scanner::{scan_imports_with_kind, ImportKind};
use crate::tsconfig::ResolvedTsConfig;

/// The file dependency graph for an Angular project.
#[derive(Debug)]
pub struct FileGraph {
    /// The directed graph where nodes are file paths and edges are import relationships.
    /// Edge weight indicates whether the import is static or dynamic.
    pub graph: DiGraph<PathBuf, ImportKind>,
    /// Map from canonical file path to its node index, for O(1) lookup.
    pub path_index: HashMap<PathBuf, NodeIndex>,
    /// Files with in-degree 0 (no other project file imports them).
    pub entry_points: Vec<PathBuf>,
    /// Import specifiers that could not be resolved to a file.
    pub unresolved: Vec<UnresolvedImport>,
    /// Bare module specifiers (npm packages) and the project files that import them.
    /// Used for npm resolution: maps specifier → list of (importing_file, import_kind).
    pub npm_import_sites: HashMap<String, Vec<(PathBuf, ImportKind)>>,
}

/// Record of an import that could not be resolved to a file on disk.
#[derive(Debug, Clone)]
pub struct UnresolvedImport {
    /// The file containing the import statement.
    pub from_file: PathBuf,
    /// The raw import specifier.
    pub specifier: String,
}

/// Summary statistics for display by the CLI.
#[derive(Debug, Clone)]
pub struct GraphSummary {
    /// Total number of files (nodes) in the graph.
    pub file_count: usize,
    /// Number of entry point files.
    pub entry_point_count: usize,
    /// Total number of edges (import relationships) in the graph.
    pub edge_count: usize,
    /// Number of imports that could not be resolved.
    pub unresolved_count: usize,
}

/// Configuration for the import resolver, derived from tsconfig.
#[derive(Debug, Clone)]
struct ResolverConfig {
    /// The root directory of the project (directory containing tsconfig.json).
    root_dir: PathBuf,
    /// The base URL for resolving non-relative imports.
    base_url: Option<PathBuf>,
    /// Path alias mappings.
    path_aliases: HashMap<String, Vec<String>>,
}

impl ResolverConfig {
    /// Build resolver config from a resolved tsconfig.
    fn from_tsconfig(config: &ResolvedTsConfig) -> NgcResult<Self> {
        let root_dir = config
            .config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let base_url = config
            .compiler_options
            .base_url
            .as_ref()
            .map(|b| root_dir.join(b));

        let path_aliases = config.compiler_options.paths.clone().unwrap_or_default();

        Ok(ResolverConfig {
            root_dir,
            base_url,
            path_aliases,
        })
    }
}

/// Build the file dependency graph for a project.
///
/// Starting from the files matched by the tsconfig `include` globs, scans all
/// TypeScript files for import statements and constructs a directed graph of
/// file dependencies. File scanning is parallelized using rayon.
pub fn build_file_graph(config: &ResolvedTsConfig) -> NgcResult<FileGraph> {
    let resolver_config = ResolverConfig::from_tsconfig(config)?;
    let discovered_files = discover_files(config)?;

    debug!(
        file_count = discovered_files.len(),
        "discovered project files"
    );

    let mut graph = DiGraph::new();
    let mut path_index: HashMap<PathBuf, NodeIndex> = HashMap::new();

    // Add all discovered files as nodes
    for file in &discovered_files {
        let idx = graph.add_node(file.clone());
        path_index.insert(file.clone(), idx);
    }

    // Parallel scan: read each file and extract imports with kind
    let scan_results: Vec<(PathBuf, Vec<crate::import_scanner::ScannedImport>)> = discovered_files
        .par_iter()
        .filter_map(|file_path| {
            let contents = std::fs::read_to_string(file_path).ok()?;
            let imports = scan_imports_with_kind(&contents);
            Some((file_path.clone(), imports))
        })
        .collect();

    let mut unresolved = Vec::new();
    let mut npm_import_sites: HashMap<String, Vec<(PathBuf, ImportKind)>> = HashMap::new();

    // Resolve imports and add edges (single-threaded for graph mutation)
    for (from_file, scanned_imports) in &scan_results {
        let from_idx = path_index[from_file];
        for scanned in scanned_imports {
            match resolve_specifier(&scanned.specifier, from_file, &resolver_config) {
                Some(resolved_path) => {
                    if let Some(&to_idx) = path_index.get(&resolved_path) {
                        graph.add_edge(from_idx, to_idx, scanned.kind);
                    }
                    // If resolved but not in our file set, it's an external file — skip
                }
                None => {
                    if is_project_local(&scanned.specifier, &resolver_config) {
                        // Project-local import that failed to resolve
                        unresolved.push(UnresolvedImport {
                            from_file: from_file.clone(),
                            specifier: scanned.specifier.clone(),
                        });
                    } else {
                        // Bare module specifier — record for npm resolution
                        npm_import_sites
                            .entry(scanned.specifier.clone())
                            .or_default()
                            .push((from_file.clone(), scanned.kind));
                    }
                }
            }
        }
    }

    // Entry points are nodes with in-degree 0
    let entry_points: Vec<PathBuf> = graph
        .node_indices()
        .filter(|&idx| {
            graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .next()
                .is_none()
        })
        .map(|idx| graph[idx].clone())
        .collect();

    debug!(
        entry_point_count = entry_points.len(),
        edge_count = graph.edge_count(),
        "graph built"
    );

    Ok(FileGraph {
        graph,
        path_index,
        entry_points,
        unresolved,
        npm_import_sites,
    })
}

/// Compute summary statistics from a file graph.
pub fn summarize(graph: &FileGraph) -> GraphSummary {
    GraphSummary {
        file_count: graph.graph.node_count(),
        entry_point_count: graph.entry_points.len(),
        edge_count: graph.graph.edge_count(),
        unresolved_count: graph.unresolved.len(),
    }
}

/// Discover all TypeScript files matching the tsconfig include/exclude patterns.
fn discover_files(config: &ResolvedTsConfig) -> NgcResult<Vec<PathBuf>> {
    let root_dir = config
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."));

    let mut files = Vec::new();

    // If explicit files list is provided, use that
    if !config.files.is_empty() {
        for f in &config.files {
            let path = root_dir.join(f);
            if path.exists() {
                let canonical = path.canonicalize().map_err(|e| NgcError::Io {
                    path: path.clone(),
                    source: e,
                })?;
                files.push(canonical);
            }
        }
        return Ok(files);
    }

    // Otherwise, use include globs
    let exclude_patterns: Vec<glob::Pattern> = config
        .exclude
        .iter()
        .filter_map(|p| {
            let full = root_dir.join(p);
            glob::Pattern::new(full.to_str().unwrap_or("")).ok()
        })
        .collect();

    for pattern in &config.include {
        let full_pattern = root_dir.join(pattern);
        let pattern_str = full_pattern.to_str().unwrap_or("");

        let entries = glob::glob(pattern_str).map_err(|e| NgcError::InvalidPathAlias {
            pattern: format!("{pattern}: {e}"),
        })?;

        for entry in entries {
            let path = entry.map_err(|e| NgcError::Io {
                path: PathBuf::from(pattern),
                source: e.into_error(),
            })?;

            // Check against exclude patterns
            let excluded = exclude_patterns.iter().any(|ex| ex.matches_path(&path));

            if !excluded {
                let canonical = path.canonicalize().map_err(|e| NgcError::Io {
                    path: path.clone(),
                    source: e,
                })?;
                files.push(canonical);
            }
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

/// Check if a specifier looks like a project-local import (not a bare module).
fn is_project_local(specifier: &str, config: &ResolverConfig) -> bool {
    if specifier.starts_with('.') {
        return true;
    }
    // Check if it matches any path alias
    config
        .path_aliases
        .keys()
        .any(|alias| matches_alias(specifier, alias))
}

/// Check if a specifier matches a path alias pattern.
fn matches_alias(specifier: &str, alias: &str) -> bool {
    if let Some(prefix) = alias.strip_suffix('*') {
        specifier.starts_with(prefix)
    } else {
        specifier == alias
    }
}

/// Resolve an import specifier to an absolute file path.
///
/// Returns `None` for bare module imports (node_modules packages).
fn resolve_specifier(
    specifier: &str,
    from_file: &Path,
    config: &ResolverConfig,
) -> Option<PathBuf> {
    // Relative imports
    if specifier.starts_with('.') {
        let from_dir = from_file.parent()?;
        return resolve_with_extensions(&from_dir.join(specifier));
    }

    // Path alias imports
    for (alias, replacements) in &config.path_aliases {
        if let Some(prefix) = alias.strip_suffix('*') {
            if let Some(rest) = specifier.strip_prefix(prefix) {
                for replacement in replacements {
                    if let Some(rep_prefix) = replacement.strip_suffix('*') {
                        let base = config.base_url.as_ref().unwrap_or(&config.root_dir);
                        let candidate = base.join(rep_prefix).join(rest);
                        if let Some(resolved) = resolve_with_extensions(&candidate) {
                            return Some(resolved);
                        }
                    }
                }
            }
        } else if specifier == alias {
            // Exact match alias
            for replacement in replacements {
                let base = config.base_url.as_ref().unwrap_or(&config.root_dir);
                let candidate = base.join(replacement);
                if let Some(resolved) = resolve_with_extensions(&candidate) {
                    return Some(resolved);
                }
            }
        }
    }

    // Bare module — return None
    None
}

/// Try to resolve a path with TypeScript extension conventions.
///
/// Tries in order: exact path, `{base}.ts`, `{base}.tsx`, `{base}/index.ts`,
/// `{base}/index.tsx`. Uses string appending rather than `Path::with_extension`
/// to correctly handle dotted filenames like `app.component`.
fn resolve_with_extensions(base: &Path) -> Option<PathBuf> {
    // Exact path
    if base.is_file() {
        return base.canonicalize().ok();
    }

    let base_str = base.as_os_str().to_str()?;

    // Try .ts extension (append, don't replace)
    let with_ts = PathBuf::from(format!("{base_str}.ts"));
    if with_ts.is_file() {
        return with_ts.canonicalize().ok();
    }

    // Try .tsx extension
    let with_tsx = PathBuf::from(format!("{base_str}.tsx"));
    if with_tsx.is_file() {
        return with_tsx.canonicalize().ok();
    }

    // Try /index.ts
    let index_ts = base.join("index.ts");
    if index_ts.is_file() {
        return index_ts.canonicalize().ok();
    }

    // Try /index.tsx
    let index_tsx = base.join("index.tsx");
    if index_tsx.is_file() {
        return index_tsx.canonicalize().ok();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/simple-app")
            .join(name)
    }

    fn fixture_resolver_config() -> ResolverConfig {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/simple-app");
        let root = root.canonicalize().unwrap();
        ResolverConfig {
            root_dir: root.clone(),
            base_url: Some(root.clone()),
            path_aliases: HashMap::from([
                ("@app/*".to_string(), vec!["src/app/*".to_string()]),
                ("@env/*".to_string(), vec!["src/environments/*".to_string()]),
            ]),
        }
    }

    #[test]
    fn test_resolve_relative_import() {
        let config = fixture_resolver_config();
        let from_file = fixture_path("src/main.ts").canonicalize().unwrap();
        let result = resolve_specifier("./app/app.component", &from_file, &config);
        assert!(result.is_some());
        let resolved = result.unwrap();
        assert!(resolved.to_str().unwrap().contains("app.component.ts"));
    }

    #[test]
    fn test_resolve_path_alias() {
        let config = fixture_resolver_config();
        let from_file = fixture_path("src/app/app.component.ts")
            .canonicalize()
            .unwrap();
        let result = resolve_specifier("@app/shared", &from_file, &config);
        assert!(result.is_some());
        let resolved = result.unwrap();
        assert!(resolved.to_str().unwrap().contains("shared"));
        assert!(resolved.to_str().unwrap().contains("index.ts"));
    }

    #[test]
    fn test_resolve_env_alias() {
        let config = fixture_resolver_config();
        let from_file = fixture_path("src/app/shared/utils.ts")
            .canonicalize()
            .unwrap();
        let result = resolve_specifier("@env/environment", &from_file, &config);
        assert!(result.is_some());
        let resolved = result.unwrap();
        assert!(resolved.to_str().unwrap().contains("environment.ts"));
    }

    #[test]
    fn test_bare_module_returns_none() {
        let config = fixture_resolver_config();
        let from_file = fixture_path("src/main.ts").canonicalize().unwrap();
        assert!(resolve_specifier("@angular/core", &from_file, &config).is_none());
        assert!(resolve_specifier("@angular/router", &from_file, &config).is_none());
        assert!(resolve_specifier("rxjs", &from_file, &config).is_none());
    }

    #[test]
    fn test_index_ts_resolution() {
        let config = fixture_resolver_config();
        let from_file = fixture_path("src/app/app.component.ts")
            .canonicalize()
            .unwrap();
        let result = resolve_specifier("./shared", &from_file, &config);
        assert!(result.is_some());
        let resolved = result.unwrap();
        assert!(resolved.to_str().unwrap().ends_with("index.ts"));
    }

    #[test]
    fn test_discover_files_from_include() {
        let config = crate::tsconfig::resolve_tsconfig(&fixture_path("tsconfig.app.json")).unwrap();
        let files = discover_files(&config).unwrap();
        assert_eq!(files.len(), 13);

        // Verify all expected files are present
        let file_names: Vec<String> = files
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .collect();
        assert!(file_names.contains(&"main.ts".to_string()));
        assert!(file_names.contains(&"app.component.ts".to_string()));
        assert!(file_names.contains(&"environment.prod.ts".to_string()));
    }

    #[test]
    fn test_build_file_graph_simple_app() {
        let config = crate::tsconfig::resolve_tsconfig(&fixture_path("tsconfig.app.json")).unwrap();
        let file_graph = build_file_graph(&config).unwrap();
        let summary = summarize(&file_graph);

        assert_eq!(summary.file_count, 13);
        assert_eq!(summary.edge_count, 13);
        assert_eq!(summary.unresolved_count, 0);
        assert_eq!(summary.entry_point_count, 2);
    }

    #[test]
    fn test_is_project_local() {
        let config = fixture_resolver_config();
        assert!(is_project_local("./foo", &config));
        assert!(is_project_local("../bar", &config));
        assert!(is_project_local("@app/shared", &config));
        assert!(is_project_local("@env/environment", &config));
        assert!(!is_project_local("@angular/core", &config));
        assert!(!is_project_local("rxjs", &config));
    }
}
