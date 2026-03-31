use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use ngc_project_resolver::ImportKind;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::{Dfs, EdgeRef};
use tracing::debug;

/// Identifies the kind of chunk in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkKind {
    /// The main entry chunk.
    Main,
    /// A lazy-loaded chunk triggered by a dynamic import.
    Lazy,
    /// A shared chunk extracted because multiple lazy chunks import it.
    Shared,
}

/// A chunk is a set of modules that will be bundled into one output file.
#[derive(Debug)]
pub struct Chunk {
    /// The kind of chunk.
    pub kind: ChunkKind,
    /// The output filename (e.g. `"main.js"`, `"chunk-admin-component.js"`).
    pub filename: String,
    /// The canonical paths of modules in this chunk, in topological order.
    pub modules: Vec<PathBuf>,
    /// The entry module for this chunk (for lazy chunks, the dynamic import target).
    pub entry: PathBuf,
}

/// The result of chunk graph construction.
#[derive(Debug)]
pub struct ChunkGraph {
    /// All chunks, with the main chunk always at index 0.
    pub chunks: Vec<Chunk>,
    /// Map from dynamic import target path to its chunk filename.
    pub dynamic_import_map: HashMap<PathBuf, String>,
}

/// Build a chunk graph by partitioning modules into main, lazy, and shared chunks.
///
/// Detects dynamic import edges as split points, then assigns each module to
/// exactly one chunk based on static reachability analysis.
pub fn build_chunk_graph(
    graph: &DiGraph<PathBuf, ImportKind>,
    entry: &PathBuf,
    root_dir: &Path,
) -> NgcResult<ChunkGraph> {
    // Build path -> node index map
    let mut path_to_idx: HashMap<PathBuf, NodeIndex> = HashMap::new();
    for idx in graph.node_indices() {
        path_to_idx.insert(graph[idx].clone(), idx);
    }

    let entry_idx = path_to_idx.get(entry).ok_or_else(|| NgcError::ChunkError {
        message: format!(
            "entry point {} not found in the dependency graph",
            entry.display()
        ),
    })?;

    // Step 1: Identify split points (targets of dynamic import edges)
    let mut split_points: BTreeSet<NodeIndex> = BTreeSet::new();
    for edge in graph.edge_indices() {
        if let Some(weight) = graph.edge_weight(edge) {
            if *weight == ImportKind::Dynamic {
                if let Some((_source, target)) = graph.edge_endpoints(edge) {
                    split_points.insert(target);
                }
            }
        }
    }

    // If no dynamic imports, return a single main chunk
    if split_points.is_empty() {
        let ordered = toposort_all_reachable(graph, *entry_idx)?;
        let modules: Vec<PathBuf> = ordered.iter().map(|idx| graph[*idx].clone()).collect();
        debug!(
            module_count = modules.len(),
            "no dynamic imports, single chunk"
        );
        return Ok(ChunkGraph {
            chunks: vec![Chunk {
                kind: ChunkKind::Main,
                filename: "main.js".to_string(),
                modules,
                entry: entry.clone(),
            }],
            dynamic_import_map: HashMap::new(),
        });
    }

    // Step 2: Compute static-only reachability from main entry
    let main_reachable = static_reachable(graph, *entry_idx);

    // Step 3: Compute static-only reachability from each split point
    let mut split_reachable: HashMap<NodeIndex, HashSet<NodeIndex>> = HashMap::new();
    for &sp in &split_points {
        split_reachable.insert(sp, static_reachable(graph, sp));
    }

    // Step 4: Assign modules to chunks
    // - Module in main_reachable → main chunk
    // - Module reachable from exactly 1 split point (not in main) → that lazy chunk
    // - Module reachable from 2+ split points (not in main) → shared chunk

    // For each non-main module, track which split points can reach it
    let mut module_consumers: HashMap<NodeIndex, BTreeSet<NodeIndex>> = HashMap::new();
    for (&sp, reachable) in &split_reachable {
        for &node in reachable {
            if !main_reachable.contains(&node) {
                module_consumers.entry(node).or_default().insert(sp);
            }
        }
    }

    // Group shared modules by their consumer set
    let mut shared_groups: HashMap<BTreeSet<NodeIndex>, Vec<NodeIndex>> = HashMap::new();
    let mut lazy_exclusive: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::new();

    for (&module, consumers) in &module_consumers {
        if consumers.len() == 1 {
            let sp = *consumers.iter().next().expect("consumer set is non-empty");
            lazy_exclusive.entry(sp).or_default().push(module);
        } else {
            shared_groups
                .entry(consumers.clone())
                .or_default()
                .push(module);
        }
    }

    // Step 5: Build chunks with topological ordering

    // Main chunk
    let main_ordered = toposort_subset(graph, *entry_idx, &main_reachable)?;
    let main_modules: Vec<PathBuf> = main_ordered.iter().map(|idx| graph[*idx].clone()).collect();

    let mut chunks = vec![Chunk {
        kind: ChunkKind::Main,
        filename: "main.js".to_string(),
        modules: main_modules,
        entry: entry.clone(),
    }];

    let mut dynamic_import_map: HashMap<PathBuf, String> = HashMap::new();

    // Lazy chunks
    for &sp in &split_points {
        let sp_path = graph[sp].clone();

        // If the split point is in main_reachable, it stays in main — no lazy chunk
        if main_reachable.contains(&sp) {
            continue;
        }

        let filename = chunk_filename_from_path(&sp_path, root_dir);

        // Modules exclusive to this lazy chunk + the split point itself
        let mut chunk_nodes: HashSet<NodeIndex> = HashSet::new();
        chunk_nodes.insert(sp);
        if let Some(exclusive) = lazy_exclusive.get(&sp) {
            for &node in exclusive {
                chunk_nodes.insert(node);
            }
        }

        let ordered = toposort_subset(graph, sp, &chunk_nodes)?;
        let modules: Vec<PathBuf> = ordered.iter().map(|idx| graph[*idx].clone()).collect();

        dynamic_import_map.insert(sp_path.clone(), filename.clone());

        chunks.push(Chunk {
            kind: ChunkKind::Lazy,
            filename,
            modules,
            entry: sp_path,
        });
    }

    // Shared chunks
    let mut shared_index = 0;
    for nodes in shared_groups.values() {
        let filename = format!("chunk-shared-{shared_index}.js");
        shared_index += 1;

        let node_set: HashSet<NodeIndex> = nodes.iter().copied().collect();
        // Pick any node as "entry" for toposort — use the first in the set
        let first_node = *nodes.first().expect("shared group is non-empty");
        let ordered = toposort_subset(graph, first_node, &node_set)?;
        let modules: Vec<PathBuf> = ordered.iter().map(|idx| graph[*idx].clone()).collect();

        // Use the first module as the chunk entry
        let chunk_entry = modules.first().cloned().unwrap_or_default();

        chunks.push(Chunk {
            kind: ChunkKind::Shared,
            filename,
            modules,
            entry: chunk_entry,
        });
    }

    debug!(
        chunk_count = chunks.len(),
        lazy_count = split_points.len(),
        shared_count = shared_index,
        "built chunk graph"
    );

    Ok(ChunkGraph {
        chunks,
        dynamic_import_map,
    })
}

/// Compute the set of nodes reachable from `start` following only static edges.
fn static_reachable(graph: &DiGraph<PathBuf, ImportKind>, start: NodeIndex) -> HashSet<NodeIndex> {
    let mut visited = HashSet::new();
    let mut stack = vec![start];

    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        for edge in graph.edges(node) {
            if *edge.weight() == ImportKind::Static {
                stack.push(edge.target());
            }
        }
    }

    visited
}

/// Topological sort of all nodes reachable from `start` (following all edge kinds).
fn toposort_all_reachable(
    graph: &DiGraph<PathBuf, ImportKind>,
    start: NodeIndex,
) -> NgcResult<Vec<NodeIndex>> {
    let mut reachable = HashSet::new();
    let mut dfs = Dfs::new(graph, start);
    while let Some(node) = dfs.next(graph) {
        reachable.insert(node);
    }

    toposort_subset(graph, start, &reachable)
}

/// Topological sort of a subset of nodes within the graph.
///
/// Returns nodes in dependency-first order (leaves before roots).
/// Handles cycles gracefully by using DFS post-order (nodes in cycles
/// are still included in an arbitrary but valid order).
fn toposort_subset(
    graph: &DiGraph<PathBuf, ImportKind>,
    _start: NodeIndex,
    subset: &HashSet<NodeIndex>,
) -> NgcResult<Vec<NodeIndex>> {
    // Try strict toposort first
    match petgraph::algo::toposort(graph, None) {
        Ok(topo) => {
            let mut ordered: Vec<NodeIndex> = topo
                .into_iter()
                .filter(|idx| subset.contains(idx))
                .collect();
            ordered.reverse();
            Ok(ordered)
        }
        Err(_) => {
            // Graph has cycles (common with npm packages) — use DFS post-order
            // which gives a valid ordering even with cycles
            let mut visited = HashSet::new();
            let mut order = Vec::new();
            for &node in subset {
                dfs_postorder(graph, node, subset, &mut visited, &mut order);
            }
            Ok(order)
        }
    }
}

/// DFS post-order traversal for cycle-tolerant topological sorting.
fn dfs_postorder(
    graph: &DiGraph<PathBuf, ImportKind>,
    node: NodeIndex,
    subset: &HashSet<NodeIndex>,
    visited: &mut HashSet<NodeIndex>,
    order: &mut Vec<NodeIndex>,
) {
    if !visited.insert(node) {
        return;
    }
    for neighbor in graph.neighbors(node) {
        if subset.contains(&neighbor) {
            dfs_postorder(graph, neighbor, subset, visited, order);
        }
    }
    order.push(node);
}

/// Derive a chunk filename from a split point's file path.
///
/// Example: `/root/src/app/admin/admin.component.ts` → `"chunk-admin-component.js"`
fn chunk_filename_from_path(path: &Path, root_dir: &Path) -> String {
    let relative = path.strip_prefix(root_dir).unwrap_or(path);
    let stem = relative.with_extension("");
    let stem_str = stem.to_string_lossy();

    // Take only the filename part (last component), sanitize it
    let name = stem_str
        .replace(['/', '\\'], "-")
        .replace('.', "-")
        .to_lowercase();

    // Take the last two path segments for a more readable name
    let parts: Vec<&str> = name.split('-').filter(|s| !s.is_empty()).collect();
    let short_name = if parts.len() > 2 {
        parts[parts.len() - 2..].join("-")
    } else {
        parts.join("-")
    };

    format!("chunk-{short_name}.js")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_path(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn test_no_dynamic_imports_single_chunk() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/src/leaf.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, leaf, ImportKind::Static);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        assert_eq!(result.chunks.len(), 1);
        assert_eq!(result.chunks[0].kind, ChunkKind::Main);
        assert_eq!(result.chunks[0].filename, "main.js");
        assert!(result.dynamic_import_map.is_empty());
    }

    #[test]
    fn test_one_dynamic_import_creates_lazy_chunk() {
        let mut graph = DiGraph::new();
        let lazy = graph.add_node(make_path("/root/src/admin/admin.component.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, lazy, ImportKind::Dynamic);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        assert_eq!(result.chunks.len(), 2);
        assert_eq!(result.chunks[0].kind, ChunkKind::Main);
        assert_eq!(result.chunks[1].kind, ChunkKind::Lazy);
        assert!(result.chunks[1].filename.contains("chunk-"));
        assert!(result.chunks[1].filename.ends_with(".js"));
        assert_eq!(result.dynamic_import_map.len(), 1);
    }

    #[test]
    fn test_shared_dependency_creates_shared_chunk() {
        // main --dynamic--> admin
        // main --dynamic--> dashboard
        // admin --static--> shared
        // dashboard --static--> shared
        let mut graph = DiGraph::new();
        let shared = graph.add_node(make_path("/root/src/shared/shared.service.ts"));
        let admin = graph.add_node(make_path("/root/src/admin/admin.component.ts"));
        let dashboard = graph.add_node(make_path("/root/src/dashboard/dashboard.component.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));

        graph.add_edge(entry, admin, ImportKind::Dynamic);
        graph.add_edge(entry, dashboard, ImportKind::Dynamic);
        graph.add_edge(admin, shared, ImportKind::Static);
        graph.add_edge(dashboard, shared, ImportKind::Static);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        // Should have: main + 2 lazy + 1 shared = 4 chunks
        assert_eq!(result.chunks.len(), 4);
        assert_eq!(
            result
                .chunks
                .iter()
                .filter(|c| c.kind == ChunkKind::Main)
                .count(),
            1
        );
        assert_eq!(
            result
                .chunks
                .iter()
                .filter(|c| c.kind == ChunkKind::Lazy)
                .count(),
            2
        );
        assert_eq!(
            result
                .chunks
                .iter()
                .filter(|c| c.kind == ChunkKind::Shared)
                .count(),
            1
        );

        // Shared chunk should contain the shared module
        let shared_chunk = result
            .chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Shared)
            .expect("should have shared chunk");
        assert!(shared_chunk
            .modules
            .iter()
            .any(|m| m.to_str().unwrap_or("").contains("shared.service")));

        // Lazy chunks should NOT contain the shared module
        for chunk in result.chunks.iter().filter(|c| c.kind == ChunkKind::Lazy) {
            assert!(!chunk
                .modules
                .iter()
                .any(|m| m.to_str().unwrap_or("").contains("shared.service")));
        }
    }

    #[test]
    fn test_lazy_target_also_static_stays_in_main() {
        // main --static--> shared_mod
        // main --dynamic--> shared_mod (same module is also dynamically imported)
        let mut graph = DiGraph::new();
        let shared_mod = graph.add_node(make_path("/root/src/shared.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));

        graph.add_edge(entry, shared_mod, ImportKind::Static);
        graph.add_edge(entry, shared_mod, ImportKind::Dynamic);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        // shared_mod is statically reachable from main → stays in main, no lazy chunk
        assert_eq!(result.chunks.len(), 1);
        assert_eq!(result.chunks[0].kind, ChunkKind::Main);
        assert!(result.chunks[0]
            .modules
            .iter()
            .any(|m| m.to_str().unwrap_or("").contains("shared")));
    }

    #[test]
    fn test_chunk_filename_from_path() {
        let root = Path::new("/root/src");

        assert_eq!(
            chunk_filename_from_path(Path::new("/root/src/admin/admin.component.ts"), root),
            "chunk-admin-component.js"
        );
        assert_eq!(
            chunk_filename_from_path(Path::new("/root/src/dashboard/dashboard.routes.ts"), root),
            "chunk-dashboard-routes.js"
        );
        assert_eq!(
            chunk_filename_from_path(Path::new("/root/src/lazy.ts"), root),
            "chunk-lazy.js"
        );
    }

    #[test]
    fn test_main_chunk_contains_only_statically_reachable() {
        // main --static--> routes --dynamic--> admin
        let mut graph = DiGraph::new();
        let admin = graph.add_node(make_path("/root/src/admin.ts"));
        let routes = graph.add_node(make_path("/root/src/routes.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));

        graph.add_edge(entry, routes, ImportKind::Static);
        graph.add_edge(routes, admin, ImportKind::Dynamic);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        let main_chunk = &result.chunks[0];
        assert_eq!(main_chunk.kind, ChunkKind::Main);
        // Main should have entry + routes, NOT admin
        assert!(main_chunk
            .modules
            .iter()
            .any(|m| m.to_str().unwrap_or("").contains("main")));
        assert!(main_chunk
            .modules
            .iter()
            .any(|m| m.to_str().unwrap_or("").contains("routes")));
        assert!(!main_chunk
            .modules
            .iter()
            .any(|m| m.to_str().unwrap_or("").contains("admin")));
    }
}
