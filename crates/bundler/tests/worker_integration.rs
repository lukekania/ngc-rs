//! End-to-end integration for web-worker bundling.
//!
//! Drives the full `resolve_project` → `bundle` pipeline on a synthetic project
//! that uses `new Worker(new URL('./foo.worker', import.meta.url))`, and asserts
//! that (1) a dedicated `worker-*.js` chunk is emitted containing both the worker
//! module and its nested static dependency (the worker has its own chunk graph),
//! and (2) the main bundle rewrites the URL specifier to the emitted filename.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ngc_bundler::{bundle, BundleInput, BundleOptions};
use ngc_project_resolver::resolve_project;
use tempfile::tempdir;

#[test]
fn worker_new_url_is_bundled_as_separate_chunk_and_rewritten() {
    let temp = tempdir().expect("create temp dir");
    let root = temp.path();

    // Minimal tsconfig.json — `include` globs for the synthetic sources.
    let tsconfig = r#"{
  "include": ["src/**/*.ts"],
  "exclude": []
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");

    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");

    // main.ts spawns a worker and calls a function declared inside it.
    fs::write(
        src.join("main.ts"),
        "const w = new Worker(new URL('./compute.worker', import.meta.url), { type: 'module' });\n\
         console.log(w);\n",
    )
    .expect("write main.ts");

    // compute.worker.ts statically imports a nested dependency — that nested
    // file must travel into the worker chunk, not the main chunk.
    fs::write(
        src.join("compute.worker.ts"),
        "import { heavy } from './worker-dep';\n\
         self.onmessage = (e) => self.postMessage(heavy(e.data));\n",
    )
    .expect("write compute.worker.ts");

    fs::write(
        src.join("worker-dep.ts"),
        "export function heavy(x) { return x * 2; }\n",
    )
    .expect("write worker-dep.ts");

    let file_graph = resolve_project(&root.join("tsconfig.json")).expect("resolve project");

    // The entry is main.ts — workers are not counted as top-level entry points
    // (they have an incoming Worker edge from main).
    let entry = file_graph
        .entry_points
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "main.ts"))
        .cloned()
        .expect("main.ts should be an entry point");

    // Pull the source text for each project file and hand it to the bundler
    // as-is. This test exercises the graph + bundler, not the TS transform.
    let mut modules: HashMap<PathBuf, String> = HashMap::new();
    for idx in file_graph.graph.node_indices() {
        let path = &file_graph.graph[idx];
        let source = fs::read_to_string(path).expect("read project file");
        modules.insert(path.clone(), source);
    }

    let input = BundleInput {
        modules,
        graph: file_graph.graph,
        entry,
        local_prefixes: vec![".".to_string()],
        root_dir: root.to_path_buf(),
        options: BundleOptions::default(),
        per_module_maps: HashMap::new(),
        bundled_specifiers: Default::default(),
        export_conditions: Vec::new(),
    };

    let output = bundle(&input).expect("bundle succeeds");

    // Exactly two chunks: main + worker.
    assert_eq!(
        output.chunks.len(),
        2,
        "expected main + worker chunk; got {:?}",
        output.chunks.keys().collect::<Vec<_>>()
    );

    // Locate the worker chunk by filename prefix.
    let (worker_filename, worker_code) = output
        .chunks
        .iter()
        .find(|(k, _)| k.starts_with("worker-"))
        .expect("a worker-*.js chunk should have been emitted");
    assert!(
        worker_filename.ends_with(".js"),
        "worker chunk filename should be a .js file"
    );

    // The worker chunk carries its own graph: the worker body AND its nested
    // static dependency end up inside it.
    assert!(
        worker_code.contains("onmessage"),
        "worker chunk should contain the worker module body; got:\n{worker_code}"
    );
    assert!(
        worker_code.contains("function heavy"),
        "worker chunk should inline the nested static dependency; got:\n{worker_code}"
    );

    // Main bundle: rewritten URL + no worker body.
    let main_code = output
        .chunks
        .get(&output.main_filename)
        .expect("main chunk present");
    assert!(
        main_code.contains(&format!("'./{worker_filename}'")),
        "main chunk should reference the emitted worker filename; got:\n{main_code}"
    );
    assert!(
        !main_code.contains("./compute.worker"),
        "main chunk should no longer reference the raw worker source path"
    );
    assert!(
        !main_code.contains("onmessage"),
        "main chunk must not inline the worker body"
    );
    assert!(
        !main_code.contains("function heavy"),
        "main chunk must not inline the worker's nested dependency"
    );
    // The `new Worker(new URL(..., import.meta.url), ...)` shell stays intact.
    assert!(main_code.contains("new Worker(new URL("));
    assert!(main_code.contains("import.meta.url"));
}
