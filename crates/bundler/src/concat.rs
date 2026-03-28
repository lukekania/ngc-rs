use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use ngc_diagnostics::{NgcError, NgcResult};
use petgraph::graph::DiGraph;
use petgraph::visit::Dfs;
use tracing::debug;

use crate::rewrite::{self, ExternalImport};

/// Input to the bundler.
#[derive(Debug)]
pub struct BundleInput {
    /// Map from canonical source path to transformed JS code.
    pub modules: HashMap<PathBuf, String>,
    /// The file dependency graph (nodes are canonical paths, edges are imports).
    pub graph: DiGraph<PathBuf, ()>,
    /// The entry point canonical path.
    pub entry: PathBuf,
    /// Prefixes that identify local imports (e.g. `["."]`, `[".", "@app/", "@env/"]`).
    pub local_prefixes: Vec<String>,
    /// Root directory for computing relative display paths in comments.
    pub root_dir: PathBuf,
}

/// Merge result for a single source: all imports grouped.
struct MergedImport {
    source: String,
    default_import: Option<String>,
    named_imports: BTreeSet<String>,
    is_side_effect: bool,
}

/// Bundle all modules reachable from the entry point into a single ESM string.
///
/// Topologically sorts the reachable subgraph so that dependencies appear before
/// dependents. External imports are hoisted and deduplicated at the top of the
/// bundle. Local imports and exports are stripped from each module.
pub fn bundle(input: &BundleInput) -> NgcResult<String> {
    let ordered = toposort_reachable(input)?;

    debug!(module_count = ordered.len(), "bundling modules");

    let prefix_refs: Vec<&str> = input.local_prefixes.iter().map(|s| s.as_str()).collect();
    let mut all_externals: Vec<ExternalImport> = Vec::new();
    let mut chunks: Vec<String> = Vec::new();

    for module_path in &ordered {
        let js_code = input
            .modules
            .get(module_path)
            .ok_or_else(|| NgcError::BundleError {
                message: format!(
                    "module {} is in the graph but has no transformed code",
                    module_path.display()
                ),
            })?;

        let file_name = module_path.to_string_lossy();
        let rewritten = rewrite::rewrite_module(js_code, &file_name, &prefix_refs)?;

        all_externals.extend(rewritten.external_imports);

        let trimmed = rewritten.code.trim();
        if !trimmed.is_empty() {
            let relative = module_path
                .strip_prefix(&input.root_dir)
                .unwrap_or(module_path);
            let display_path = relative.with_extension("js");
            chunks.push(format!("// {}\n{}", display_path.display(), trimmed));
        }
    }

    let merged = merge_external_imports(all_externals);
    let mut output = String::new();

    for imp in &merged {
        output.push_str(&format_import(imp));
        output.push('\n');
    }

    if !merged.is_empty() && !chunks.is_empty() {
        output.push('\n');
    }

    for (i, chunk) in chunks.iter().enumerate() {
        output.push_str(chunk);
        if i < chunks.len() - 1 {
            output.push_str("\n\n");
        } else {
            output.push('\n');
        }
    }

    Ok(output)
}

/// Compute the reachable subgraph from the entry point and return nodes
/// in topological order (dependencies first, entry last).
fn toposort_reachable(input: &BundleInput) -> NgcResult<Vec<PathBuf>> {
    // Build path -> node index map
    let mut path_to_idx = HashMap::new();
    for idx in input.graph.node_indices() {
        path_to_idx.insert(input.graph[idx].clone(), idx);
    }

    let entry_idx = path_to_idx
        .get(&input.entry)
        .ok_or_else(|| NgcError::BundleError {
            message: format!(
                "entry point {} not found in the dependency graph",
                input.entry.display()
            ),
        })?;

    // DFS to find reachable nodes
    let mut reachable = HashSet::new();
    let mut dfs = Dfs::new(&input.graph, *entry_idx);
    while let Some(node) = dfs.next(&input.graph) {
        reachable.insert(node);
    }

    // Build subgraph of reachable nodes for toposort
    let topo = petgraph::algo::toposort(&input.graph, None).map_err(|cycle| {
        let cycle_node = &input.graph[cycle.node_id()];
        NgcError::CircularDependency {
            cycle: vec![cycle_node.clone()],
        }
    })?;

    // Filter to reachable and reverse so dependencies come first (leaves before entry)
    let mut ordered: Vec<PathBuf> = topo
        .into_iter()
        .filter(|idx| reachable.contains(idx))
        .map(|idx| input.graph[idx].clone())
        .collect();
    ordered.reverse();

    debug!(
        reachable_count = ordered.len(),
        total_count = input.graph.node_count(),
        "computed bundle order"
    );

    Ok(ordered)
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
    use petgraph::graph::DiGraph;

    fn make_path(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn test_two_module_bundle() {
        let mut graph = DiGraph::new();
        let leaf = graph.add_node(make_path("/root/src/leaf.ts"));
        let entry = graph.add_node(make_path("/root/src/main.ts"));
        graph.add_edge(entry, leaf, ());

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
        };

        let result = bundle(&input).expect("should bundle");
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
        graph.add_edge(entry, a, ());
        graph.add_edge(entry, b, ());

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
        };

        let result = bundle(&input).expect("should bundle");
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
        graph.add_edge(entry, leaf, ());

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
        };

        let result = bundle(&input).expect("should bundle");
        assert!(
            !result.contains("orphan"),
            "orphan module should be excluded"
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
}
