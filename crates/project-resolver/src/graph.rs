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

    // Resolve every import in parallel — `resolve_specifier` does ~5
    // `is_file` + `canonicalize` probes per import and is the dominant cost.
    // We emit one `ResolutionOutcome` per scanned import, then serially fold
    // them into the graph (petgraph edges require `&mut`), the unresolved
    // list, and the npm-import-site map.
    enum ResolutionOutcome {
        Edge(NodeIndex, NodeIndex, ImportKind),
        Unresolved(UnresolvedImport),
        Npm {
            specifier: String,
            from_file: PathBuf,
            kind: ImportKind,
        },
        /// Resolved but points outside the file set — nothing to record.
        External,
    }

    let outcomes: Vec<ResolutionOutcome> = scan_results
        .par_iter()
        .flat_map_iter(|(from_file, scanned_imports)| {
            let from_idx = path_index[from_file];
            let resolver_config = &resolver_config;
            let path_index = &path_index;
            scanned_imports
                .iter()
                .map(move |scanned| {
                    match resolve_specifier(&scanned.specifier, from_file, resolver_config) {
                        Some(resolved_path) => {
                            if let Some(&to_idx) = path_index.get(&resolved_path) {
                                ResolutionOutcome::Edge(from_idx, to_idx, scanned.kind)
                            } else {
                                ResolutionOutcome::External
                            }
                        }
                        None => {
                            if is_project_local(&scanned.specifier, resolver_config) {
                                ResolutionOutcome::Unresolved(UnresolvedImport {
                                    from_file: from_file.clone(),
                                    specifier: scanned.specifier.clone(),
                                })
                            } else {
                                ResolutionOutcome::Npm {
                                    specifier: scanned.specifier.clone(),
                                    from_file: from_file.clone(),
                                    kind: scanned.kind,
                                }
                            }
                        }
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let mut unresolved = Vec::new();
    let mut npm_import_sites: HashMap<String, Vec<(PathBuf, ImportKind)>> = HashMap::new();
    for outcome in outcomes {
        match outcome {
            ResolutionOutcome::Edge(from_idx, to_idx, kind) => {
                graph.add_edge(from_idx, to_idx, kind);
            }
            ResolutionOutcome::Unresolved(u) => unresolved.push(u),
            ResolutionOutcome::Npm {
                specifier,
                from_file,
                kind,
            } => {
                npm_import_sites
                    .entry(specifier)
                    .or_default()
                    .push((from_file, kind));
            }
            ResolutionOutcome::External => {}
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
