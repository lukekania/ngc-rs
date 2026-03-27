use std::path::PathBuf;

use ngc_project_resolver::{resolve_project, summarize};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/simple-app")
}

#[test]
fn test_simple_app_graph_summary() {
    let fixture = fixture_root().join("tsconfig.app.json");
    let graph = resolve_project(&fixture).expect("fixture should resolve");
    let summary = summarize(&graph);
    insta::assert_snapshot!("simple_app_summary", format!("{summary:#?}"));
}

#[test]
fn test_simple_app_resolved_files() {
    let fixture = fixture_root().join("tsconfig.app.json");
    let root = fixture_root().canonicalize().unwrap();
    let graph = resolve_project(&fixture).expect("fixture should resolve");

    let mut files: Vec<String> = graph
        .graph
        .node_weights()
        .map(|p| {
            p.strip_prefix(&root)
                .unwrap_or(p)
                .to_string_lossy()
                .to_string()
        })
        .collect();
    files.sort();
    insta::assert_snapshot!("simple_app_files", files.join("\n"));
}

#[test]
fn test_simple_app_edges() {
    let fixture = fixture_root().join("tsconfig.app.json");
    let root = fixture_root().canonicalize().unwrap();
    let graph = resolve_project(&fixture).expect("fixture should resolve");

    let mut edges: Vec<String> = graph
        .graph
        .edge_indices()
        .map(|e| {
            let (a, b) = graph.graph.edge_endpoints(e).unwrap();
            let from = graph.graph[a]
                .strip_prefix(&root)
                .unwrap_or(&graph.graph[a])
                .to_string_lossy();
            let to = graph.graph[b]
                .strip_prefix(&root)
                .unwrap_or(&graph.graph[b])
                .to_string_lossy();
            format!("{from} -> {to}")
        })
        .collect();
    edges.sort();
    insta::assert_snapshot!("simple_app_edges", edges.join("\n"));
}
