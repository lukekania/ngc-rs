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

use ngc_bundler::{bundle, BundleInput, BundleOptions, BundleOutput};
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

/// Repro for issue #93: the `new Worker(new URL(...))` site is inside a class
/// constructor (Angular component shape) and the worker file lives in a parent
/// directory whose name contains `worker`. Both the rewrite and the chunk
/// filename used to break in that combination.
#[test]
fn worker_url_in_class_constructor_under_web_worker_dir_is_rewritten() {
    let temp = tempdir().expect("create temp dir");
    let root = temp.path();

    let tsconfig = r#"{
  "include": ["src/**/*.ts"],
  "exclude": []
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");

    let feature_dir = root.join("src/app/features/web-worker");
    fs::create_dir_all(&feature_dir).expect("create feature dir");
    fs::write(
        root.join("src/main.ts"),
        "import './app/features/web-worker/web-worker.component';\n",
    )
    .expect("write main.ts");

    fs::write(
        feature_dir.join("web-worker.component.ts"),
        "export class WebWorkerComponent {\n\
           worker;\n\
           constructor() {\n\
             this.worker = new Worker(new URL('./hash.worker', import.meta.url), { type: 'module' });\n\
           }\n\
         }\n",
    )
    .expect("write component");

    fs::write(
        feature_dir.join("hash.worker.ts"),
        "self.onmessage = (e) => self.postMessage(e.data);\n",
    )
    .expect("write worker");

    let file_graph = resolve_project(&root.join("tsconfig.json")).expect("resolve project");

    let entry = file_graph
        .entry_points
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "main.ts"))
        .cloned()
        .expect("main.ts should be an entry point");

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

    // Worker chunk filename must be `worker-hash.js`, not `worker-worker-hash.js`.
    let (worker_filename, worker_code) = output
        .chunks
        .iter()
        .find(|(k, _)| k.starts_with("worker-"))
        .expect("a worker-*.js chunk should have been emitted");
    assert_eq!(
        worker_filename, "worker-hash.js",
        "expected `worker-hash.js`, got `{worker_filename}` (parent dir bled into the name)",
    );
    assert!(worker_code.contains("onmessage"));

    let main_code = output
        .chunks
        .get(&output.main_filename)
        .expect("main chunk present");
    // The worker URL is inside the component class constructor — the rewriter
    // must descend into class bodies for this to land.
    assert!(
        main_code.contains("'./worker-hash.js'"),
        "main chunk should reference the worker chunk filename; got:\n{main_code}",
    );
    assert!(
        !main_code.contains("./hash.worker"),
        "main chunk must no longer reference the source-form worker specifier; got:\n{main_code}",
    );
}

/// When content hashing is on, the rewritten URL must point at the same hashed
/// filename the bundler wrote to disk — otherwise the browser fetches a 404.
#[test]
fn worker_url_rewrite_uses_content_hashed_filename() {
    let temp = tempdir().expect("create temp dir");
    let root = temp.path();

    let tsconfig = r#"{
  "include": ["src/**/*.ts"],
  "exclude": []
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");

    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");

    fs::write(
        src.join("main.ts"),
        "class App {\n\
           start() {\n\
             return new Worker(new URL('./compute.worker', import.meta.url), { type: 'module' });\n\
           }\n\
         }\n\
         new App().start();\n",
    )
    .expect("write main.ts");

    fs::write(
        src.join("compute.worker.ts"),
        "self.onmessage = (e) => self.postMessage(e.data * 2);\n",
    )
    .expect("write worker");

    let file_graph = resolve_project(&root.join("tsconfig.json")).expect("resolve project");

    let entry = file_graph
        .entry_points
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "main.ts"))
        .cloned()
        .expect("main.ts should be an entry point");

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
        options: BundleOptions {
            content_hash: true,
            ..BundleOptions::default()
        },
        per_module_maps: HashMap::new(),
        bundled_specifiers: Default::default(),
        export_conditions: Vec::new(),
    };

    let output: BundleOutput = bundle(&input).expect("bundle succeeds");

    let worker_filename = output
        .chunks
        .keys()
        .find(|k| k.starts_with("worker-"))
        .cloned()
        .expect("worker chunk should exist");

    // Hashed filename has the form `worker-compute.<8-hex>.js`.
    assert!(
        worker_filename.starts_with("worker-compute.") && worker_filename.ends_with(".js"),
        "worker chunk filename `{worker_filename}` should look like `worker-compute.<hash>.js`",
    );

    let main_code = output
        .chunks
        .get(&output.main_filename)
        .expect("main chunk present");
    assert!(
        main_code.contains(&format!("'./{worker_filename}'")),
        "main chunk should reference the hashed worker filename `{worker_filename}`; got:\n{main_code}",
    );
    assert!(
        !main_code.contains("./compute.worker"),
        "main chunk must not contain the source-form specifier",
    );
}
