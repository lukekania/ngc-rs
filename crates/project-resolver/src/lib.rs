pub mod graph;
pub mod import_scanner;
pub mod tsconfig;

use std::path::Path;

use graph::{FileGraph, GraphSummary};
use ngc_diagnostics::NgcResult;

/// Resolve the complete file dependency graph for an Angular project.
///
/// Given a path to a tsconfig.json, parses the config (following extends
/// chains), discovers entry point files, scans all TypeScript files for
/// imports, and builds a directed dependency graph.
pub fn resolve_project(tsconfig_path: &Path) -> NgcResult<FileGraph> {
    let config = tsconfig::resolve_tsconfig(tsconfig_path)?;
    graph::build_file_graph(&config)
}

/// Compute summary statistics for a resolved file graph.
pub fn summarize(graph: &FileGraph) -> GraphSummary {
    graph::summarize(graph)
}
