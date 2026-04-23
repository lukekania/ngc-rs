//! End-to-end integration for `@defer` block chunk splitting.
//!
//! Drives the full `resolve_project` → `bundle` pipeline on a synthetic
//! project whose component template would (post-template-compile) emit a
//! dependency resolver function containing `import('./deferred.component')`
//! — the shape ngc-rs's template-compiler produces for `@defer` blocks.
//!
//! Asserts that (1) the deferred component lands in its own lazy chunk,
//! (2) the `@placeholder` / `@loading` / `@error` components — which are
//! referenced statically from the main component for immediate rendering
//! — stay in the main chunk, and (3) the main chunk references the
//! emitted lazy chunk's filename (so the runtime `import()` resolves at
//! load time).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ngc_bundler::{bundle, BundleInput, BundleOptions};
use ngc_project_resolver::resolve_project;
use tempfile::tempdir;

#[test]
fn defer_deferred_component_is_chunk_split_placeholder_stays_in_main() {
    let temp = tempdir().expect("create temp dir");
    let root = temp.path();

    let tsconfig = r#"{
  "include": ["src/**/*.ts"],
  "exclude": []
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");

    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");

    // main.ts — statically imports the root component.
    fs::write(
        src.join("main.ts"),
        "import { AppComponent } from './app.component';\n\
         console.log(AppComponent);\n",
    )
    .expect("write main.ts");

    // app.component.ts — the shape the template-compiler emits for a
    // template containing:
    //   @defer (on idle) { <deferred-cmp /> }
    //   @placeholder { <placeholder-cmp /> }
    //   @loading { <loading-cmp /> }
    //   @error { <error-cmp /> }
    //
    // Placeholder, loading, and error are rendered before/during/after the
    // deferred resource loads, so they must be available at the moment the
    // parent component renders — hence their static imports. The deferred
    // component is loaded on demand via the dep fn's dynamic import.
    fs::write(
        src.join("app.component.ts"),
        "import { PlaceholderCmp } from './placeholder.component';\n\
         import { LoadingCmp } from './loading.component';\n\
         import { ErrorCmp } from './error.component';\n\
         function AppComponent_Defer_0_DepsFn() {\n\
           return [import('./deferred.component').then(m => m.DeferredCmp)];\n\
         }\n\
         export class AppComponent {\n\
           placeholder = PlaceholderCmp;\n\
           loading = LoadingCmp;\n\
           error = ErrorCmp;\n\
           depsFn = AppComponent_Defer_0_DepsFn;\n\
         }\n",
    )
    .expect("write app.component.ts");

    fs::write(
        src.join("deferred.component.ts"),
        "export class DeferredCmp {}\n",
    )
    .expect("write deferred.component.ts");

    fs::write(
        src.join("placeholder.component.ts"),
        "export class PlaceholderCmp {}\n",
    )
    .expect("write placeholder.component.ts");

    fs::write(
        src.join("loading.component.ts"),
        "export class LoadingCmp {}\n",
    )
    .expect("write loading.component.ts");

    fs::write(src.join("error.component.ts"), "export class ErrorCmp {}\n")
        .expect("write error.component.ts");

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

    // Main + one lazy chunk for the deferred component = 2 chunks.
    assert_eq!(
        output.chunks.len(),
        2,
        "expected main + deferred chunk; got {:?}",
        output.chunks.keys().collect::<Vec<_>>()
    );

    // Locate the deferred chunk by filename.
    let (lazy_filename, lazy_code) = output
        .chunks
        .iter()
        .find(|(k, _)| k.as_str() != output.main_filename)
        .expect("a lazy chunk for the deferred component should exist");
    assert!(
        lazy_filename.contains("deferred"),
        "lazy chunk filename should reference the deferred component: {lazy_filename}"
    );
    assert!(
        lazy_code.contains("DeferredCmp"),
        "lazy chunk should carry the deferred class: {lazy_code}"
    );

    // Main chunk: placeholder/loading/error components inlined.
    let main_code = output
        .chunks
        .get(&output.main_filename)
        .expect("main chunk present");
    assert!(
        main_code.contains("PlaceholderCmp"),
        "main chunk should inline PlaceholderCmp: {main_code}"
    );
    assert!(
        main_code.contains("LoadingCmp"),
        "main chunk should inline LoadingCmp: {main_code}"
    );
    assert!(
        main_code.contains("ErrorCmp"),
        "main chunk should inline ErrorCmp: {main_code}"
    );
    // DeferredCmp must NOT be in the main chunk.
    assert!(
        !main_code.contains("class DeferredCmp"),
        "main chunk must not inline DeferredCmp: {main_code}"
    );
    // The import() specifier in the dep fn is rewritten to the lazy chunk path.
    assert!(
        main_code.contains(&format!("'./{lazy_filename}'")),
        "main chunk should reference the deferred chunk filename: {main_code}"
    );
}
