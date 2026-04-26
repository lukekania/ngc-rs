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

    // Step 1: Identify split points (targets of dynamic import or worker edges)
    // and remember which ones came from worker URLs so we can give them a
    // `worker-` filename prefix instead of the generic `chunk-` prefix.
    let mut split_points: BTreeSet<NodeIndex> = BTreeSet::new();
    let mut worker_split_points: HashSet<NodeIndex> = HashSet::new();
    for edge in graph.edge_indices() {
        if let Some(weight) = graph.edge_weight(edge) {
            match *weight {
                ImportKind::Dynamic => {
                    if let Some((_source, target)) = graph.edge_endpoints(edge) {
                        split_points.insert(target);
                    }
                }
                ImportKind::Worker => {
                    if let Some((_source, target)) = graph.edge_endpoints(edge) {
                        split_points.insert(target);
                        worker_split_points.insert(target);
                    }
                }
                ImportKind::Static => {}
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
    let mut main_reachable = static_reachable(graph, *entry_idx);

    // Force npm modules that are reachable from ANY entry point (main or split)
    // into the main chunk.  The bundler's IIFE wrapping and namespace system
    // only operates on the main chunk — npm modules that end up in lazy/shared
    // chunks would be silently omitted, producing missing cross-chunk exports.
    // We compute full reachability (all edge types) from the main entry to find
    // all npm modules the app actually uses, excluding unreachable npm modules
    // (e.g. test-only packages discovered by scanning non-entry files).
    let all_reachable = all_reachable(graph, *entry_idx);
    for idx in all_reachable {
        if graph[idx]
            .components()
            .any(|c| c.as_os_str() == "node_modules")
        {
            main_reachable.insert(idx);
        }
    }

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
    let shared_groups: HashMap<BTreeSet<NodeIndex>, Vec<NodeIndex>> = HashMap::new();
    let mut lazy_exclusive: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::new();

    for (&module, consumers) in &module_consumers {
        if consumers.len() == 1 {
            let sp = *consumers.iter().next().expect("consumer set is non-empty");
            lazy_exclusive.entry(sp).or_default().push(module);
        } else {
            // Modules needed by multiple lazy chunks go to the main chunk
            // so they can be exported via `./main.js` cross-chunk imports.
            // Putting them in separate shared chunks would require cross-chunk
            // import resolution between non-main chunks, which isn't supported yet.
            main_reachable.insert(module);
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

        let filename = if worker_split_points.contains(&sp) {
            worker_chunk_filename_from_path(&sp_path, root_dir)
        } else {
            chunk_filename_from_path(&sp_path, root_dir)
        };

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

/// Compute the set of nodes reachable from `start` following all edge types.
fn all_reachable(graph: &DiGraph<PathBuf, ImportKind>, start: NodeIndex) -> HashSet<NodeIndex> {
    let mut visited = HashSet::new();
    let mut stack = vec![start];

    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        for neighbor in graph.neighbors(node) {
            stack.push(neighbor);
        }
    }

    visited
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
    // Always build a subgraph of the subset nodes and sort that.
    // Filtering a global toposort is incorrect: the global order may
    // interleave non-subset nodes that cause two subset nodes to appear
    // in the wrong relative order.
    toposort_subset_with_cycles(graph, subset)
}

/// Topological sort of a subset that may contain cycles.
///
/// Builds a subgraph of the subset nodes, condenses SCCs into single nodes
/// (creating a DAG), toposorts the DAG, then expands SCCs back into their
/// constituent nodes.  Within each SCC, nodes are ordered by DFS post-order.
fn toposort_subset_with_cycles(
    graph: &DiGraph<PathBuf, ImportKind>,
    subset: &HashSet<NodeIndex>,
) -> NgcResult<Vec<NodeIndex>> {
    use petgraph::algo::kosaraju_scc;

    // Build a subgraph containing only subset nodes
    let mut sub = DiGraph::<NodeIndex, ()>::new();
    let mut orig_to_sub: HashMap<NodeIndex, NodeIndex> = HashMap::new();

    for &node in subset {
        let sub_node = sub.add_node(node);
        orig_to_sub.insert(node, sub_node);
    }
    for &node in subset {
        let sub_from = orig_to_sub[&node];
        for neighbor in graph.neighbors(node) {
            if let Some(&sub_to) = orig_to_sub.get(&neighbor) {
                sub.add_edge(sub_from, sub_to, ());
            }
        }
    }

    // Find SCCs using Kosaraju's algorithm
    let sccs = kosaraju_scc(&sub);

    // Debug: log large SCCs
    for (i, scc) in sccs.iter().enumerate() {
        if scc.len() > 5 {
            let paths: Vec<String> = scc
                .iter()
                .take(10)
                .map(|&n| graph[sub[n]].to_string_lossy().to_string())
                .collect();
            tracing::debug!(scc_index = i, size = scc.len(), sample = ?paths, "large SCC detected");
        }
    }

    // Map each sub-node to its SCC index
    let mut node_to_scc: HashMap<NodeIndex, usize> = HashMap::new();
    for (scc_idx, scc) in sccs.iter().enumerate() {
        for &sub_node in scc {
            node_to_scc.insert(sub_node, scc_idx);
        }
    }

    // Build condensation DAG: each SCC becomes a single node
    let mut cond = DiGraph::<usize, ()>::new();
    let mut scc_to_cond: HashMap<usize, NodeIndex> = HashMap::new();
    for i in 0..sccs.len() {
        let cond_node = cond.add_node(i);
        scc_to_cond.insert(i, cond_node);
    }
    let mut cond_edges_seen: HashSet<(usize, usize)> = HashSet::new();
    for edge in sub.edge_indices() {
        if let Some((from, to)) = sub.edge_endpoints(edge) {
            let scc_from = node_to_scc[&from];
            let scc_to = node_to_scc[&to];
            if scc_from != scc_to && cond_edges_seen.insert((scc_from, scc_to)) {
                let cf = scc_to_cond[&scc_from];
                let ct = scc_to_cond[&scc_to];
                cond.add_edge(cf, ct, ());
            }
        }
    }

    // Deterministic toposort of the condensation DAG using Kahn's algorithm
    // with path-based tie-breaking.  petgraph::algo::toposort uses DFS which
    // is non-deterministic (depends on node insertion order that can vary
    // between builds/environments).
    let cond_order = {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut in_degree: HashMap<NodeIndex, usize> = HashMap::new();
        for node in cond.node_indices() {
            in_degree.entry(node).or_insert(0);
            for neighbor in cond.neighbors(node) {
                *in_degree.entry(neighbor).or_insert(0) += 1;
            }
        }
        // Build a stable sort key for each condensation node using the
        // smallest file path in the SCC. This ensures deterministic ordering
        // regardless of graph insertion order or SCC numbering.
        let mut cond_sort_key: HashMap<NodeIndex, String> = HashMap::new();
        for cond_node in cond.node_indices() {
            let scc_idx = cond[cond_node];
            let scc = &sccs[scc_idx];
            let min_path = scc
                .iter()
                .map(|&sub_node| {
                    let orig_idx = sub[sub_node];
                    graph[orig_idx].to_string_lossy().to_string()
                })
                .min()
                .unwrap_or_default();
            cond_sort_key.insert(cond_node, min_path);
        }
        let mut queue: BinaryHeap<Reverse<(String, NodeIndex)>> = BinaryHeap::new();
        for (&node, &deg) in &in_degree {
            if deg == 0 {
                let key = cond_sort_key.get(&node).cloned().unwrap_or_default();
                queue.push(Reverse((key, node)));
            }
        }
        let mut order = Vec::new();
        while let Some(Reverse((_, node))) = queue.pop() {
            order.push(node);
            for neighbor in cond.neighbors(node) {
                let deg = in_degree.get_mut(&neighbor).expect("node in graph");
                *deg -= 1;
                if *deg == 0 {
                    let key = cond_sort_key.get(&neighbor).cloned().unwrap_or_default();
                    queue.push(Reverse((key, neighbor)));
                }
            }
        }
        if order.len() != cond.node_count() {
            return Err(NgcError::ChunkError {
                message: "condensation graph has a cycle (should be impossible)".into(),
            });
        }
        order
    };

    // Expand: for each SCC in reverse toposort order (dependencies first),
    // emit the nodes within the SCC.  Within each SCC, use DFS post-order
    // on the subgraph to get the best possible internal ordering.
    let mut result = Vec::with_capacity(subset.len());
    for cond_node in cond_order.into_iter().rev() {
        let scc_idx = cond[cond_node];
        let scc = &sccs[scc_idx];
        if scc.len() == 1 {
            result.push(sub[scc[0]]);
        } else {
            // For multi-node SCCs, do a local DFS post-order within the SCC
            // so that within-SCC dependencies are ordered as well as possible.
            // Sort SCC nodes by path for deterministic iteration order
            // (Tarjan's algorithm doesn't guarantee stable ordering).
            let scc_set: HashSet<NodeIndex> = scc.iter().copied().collect();
            let mut sorted_scc: Vec<NodeIndex> = scc.to_vec();
            sorted_scc.sort_by(|a, b| {
                let pa = graph[sub[*a]].to_string_lossy();
                let pb = graph[sub[*b]].to_string_lossy();
                pa.cmp(&pb)
            });
            let mut emitted = HashSet::new();
            let mut in_progress = HashSet::new();
            let mut local_order = Vec::new();
            for &scc_node in &sorted_scc {
                scc_emit_deps_first(
                    &sub,
                    scc_node,
                    &scc_set,
                    &mut emitted,
                    &mut in_progress,
                    &mut local_order,
                    graph,
                );
            }
            for sub_node in local_order {
                result.push(sub[sub_node]);
            }
        }
    }

    Ok(result)
}

/// Emit SCC nodes in dependency-first order (best-effort for cycles).
///
/// For each unfinished node, first recursively emit all its unfinished
/// in-SCC dependencies.  Back edges (cycles) are detected by tracking
/// nodes currently on the recursion path.
fn scc_emit_deps_first(
    sub: &DiGraph<NodeIndex, ()>,
    node: NodeIndex,
    scc_set: &HashSet<NodeIndex>,
    emitted: &mut HashSet<NodeIndex>,
    in_progress: &mut HashSet<NodeIndex>,
    order: &mut Vec<NodeIndex>,
    graph: &DiGraph<std::path::PathBuf, crate::chunk::ImportKind>,
) {
    if emitted.contains(&node) {
        return;
    }
    if !in_progress.insert(node) {
        // Cycle detected — break it by skipping
        return;
    }
    // Emit all in-SCC dependencies first.
    // Sort neighbors by path for deterministic ordering across environments.
    let mut neighbors: Vec<NodeIndex> = sub
        .neighbors(node)
        .filter(|n| scc_set.contains(n) && !emitted.contains(n))
        .collect();
    neighbors.sort_by(|a, b| {
        let pa = graph[sub[*a]].to_string_lossy();
        let pb = graph[sub[*b]].to_string_lossy();
        pa.cmp(&pb)
    });
    for neighbor in neighbors {
        scc_emit_deps_first(sub, neighbor, scc_set, emitted, in_progress, order, graph);
    }
    in_progress.remove(&node);
    if emitted.insert(node) {
        order.push(node);
    }
}

/// Derive a worker chunk filename from a worker entry's file path.
///
/// Strips a trailing `.worker` segment so `foo.worker.ts` produces
/// `worker-foo.js` rather than the redundant `worker-foo-worker.js`.
/// Example: `/root/src/app/compute.worker.ts` → `"worker-compute.js"`
fn worker_chunk_filename_from_path(path: &Path, root_dir: &Path) -> String {
    let relative = path.strip_prefix(root_dir).unwrap_or(path);
    let stem = relative.with_extension("");
    let stem_str = stem.to_string_lossy();

    let name = stem_str
        .replace(['/', '\\'], "-")
        .replace('.', "-")
        .to_lowercase();

    let mut parts: Vec<&str> = name.split('-').filter(|s| !s.is_empty()).collect();
    if parts.last().is_some_and(|s| *s == "worker") {
        parts.pop();
    }
    let short_name = if parts.len() > 2 {
        parts[parts.len() - 2..].join("-")
    } else {
        parts.join("-")
    };

    if short_name.is_empty() {
        "worker.js".to_string()
    } else {
        format!("worker-{short_name}.js")
    }
}

/// Derive a chunk filename from a split point's file path.
///
/// Uses the file's basename stem (the filename without its trailing `.ts`/
/// `.js` extension), replacing inner `.` separators with `-`. Parent
/// directories are deliberately dropped — Angular convention puts each
/// component in a directory matching its filename, so including the parent
/// dir produces redundant `admin-admin-component`-style names.
///
/// Examples:
/// - `/root/src/admin/admin.component.ts` → `"chunk-admin-component.js"`
/// - `/root/src/dashboard/dashboard.routes.ts` → `"chunk-dashboard-routes.js"`
/// - `/root/src/service-worker/service-worker.component.ts`
///   → `"chunk-service-worker-component.js"`
/// - `/root/src/lazy.ts` → `"chunk-lazy.js"`
fn chunk_filename_from_path(path: &Path, _root_dir: &Path) -> String {
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // Strip the final extension only (e.g. `admin.component.ts` →
    // `admin.component`). Then turn the remaining `.` separators into `-`
    // so the basename `admin.component` becomes `admin-component` while
    // `service-worker.component` becomes `service-worker-component`
    // (instead of being truncated to a non-disambiguating `worker-component`).
    let stem = match basename.rsplit_once('.') {
        Some((stem, _ext)) => stem,
        None => basename,
    };
    let name = stem.replace('.', "-").to_lowercase();
    if name.is_empty() {
        "chunk.js".to_string()
    } else {
        format!("chunk-{name}.js")
    }
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

        // Should have: main + 2 lazy = 3 chunks (shared module goes to main)
        assert_eq!(result.chunks.len(), 3);
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

        // Main chunk should contain the shared module
        let main_chunk = result
            .chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Main)
            .expect("should have main chunk");
        assert!(main_chunk
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

    /// Regression: hyphenated basenames must not collapse to the same chunk
    /// name. `service-worker.component.ts` and `web-worker.component.ts` are
    /// both real routes in the test bed; the previous "last two dash-segments"
    /// rule reduced both to `chunk-worker-component.js` and one silently
    /// overwrote the other, leaving the route's `loadComponent` import
    /// resolving to `undefined`.
    #[test]
    fn test_chunk_filename_disambiguates_hyphenated_basenames() {
        let root = Path::new("/root/src");
        let sw = chunk_filename_from_path(
            Path::new("/root/src/service-worker/service-worker.component.ts"),
            root,
        );
        let ww = chunk_filename_from_path(
            Path::new("/root/src/web-worker/web-worker.component.ts"),
            root,
        );
        assert_eq!(sw, "chunk-service-worker-component.js");
        assert_eq!(ww, "chunk-web-worker-component.js");
        assert_ne!(sw, ww);
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

    #[test]
    fn test_worker_chunk_filename_from_path() {
        let root = Path::new("/root/src");
        assert_eq!(
            worker_chunk_filename_from_path(Path::new("/root/src/compute.worker.ts"), root),
            "worker-compute.js"
        );
        assert_eq!(
            worker_chunk_filename_from_path(Path::new("/root/src/workers/compute.worker.ts"), root),
            "worker-workers-compute.js"
        );
        // No trailing .worker segment — no strip.
        assert_eq!(
            worker_chunk_filename_from_path(Path::new("/root/src/foo.ts"), root),
            "worker-foo.js"
        );
    }

    #[test]
    fn test_worker_edge_creates_worker_chunk() {
        let mut graph = DiGraph::new();
        let worker = graph.add_node(make_path("/root/src/compute.worker.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, worker, ImportKind::Worker);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        assert_eq!(result.chunks.len(), 2);
        let worker_chunk = result
            .chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Lazy)
            .expect("should have a lazy chunk for the worker");
        assert_eq!(worker_chunk.filename, "worker-compute.js");
        assert_eq!(
            result
                .dynamic_import_map
                .get(&make_path("/root/src/compute.worker.ts")),
            Some(&"worker-compute.js".to_string())
        );
    }

    #[test]
    fn test_npm_modules_forced_to_main_chunk() {
        // main --dynamic--> lazy_component --static--> node_modules/cdk/overlay.mjs
        // The npm module is only reachable via a dynamic import, but it must
        // still end up in the main chunk for IIFE wrapping and cross-chunk exports.
        let mut graph = DiGraph::new();
        let npm_mod = graph.add_node(make_path(
            "/root/node_modules/@angular/cdk/fesm2022/overlay.mjs",
        ));
        let lazy = graph.add_node(make_path("/root/src/dialog/dialog.component.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));

        graph.add_edge(entry, lazy, ImportKind::Dynamic);
        graph.add_edge(lazy, npm_mod, ImportKind::Static);

        let result = build_chunk_graph(
            &graph,
            &make_path("/root/src/main.ts"),
            Path::new("/root/src"),
        )
        .expect("should build chunk graph");

        // npm module must be in the main chunk
        let main_chunk = &result.chunks[0];
        assert!(
            main_chunk
                .modules
                .iter()
                .any(|m| m.to_string_lossy().contains("node_modules")),
            "npm module should be in main chunk"
        );

        // Lazy chunk must NOT contain the npm module
        for chunk in result.chunks.iter().filter(|c| c.kind == ChunkKind::Lazy) {
            assert!(
                !chunk
                    .modules
                    .iter()
                    .any(|m| m.to_string_lossy().contains("node_modules")),
                "npm module should NOT be in lazy chunk"
            );
        }
    }
}
