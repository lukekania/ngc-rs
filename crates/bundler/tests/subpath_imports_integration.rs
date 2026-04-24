//! End-to-end integration for `package.json` `imports` subpath aliases
//! (`#`-prefixed specifiers).
//!
//! A project file imports `#internal/helper`. The project's `package.json`
//! declares an `imports` map that rewrites the alias to a file under
//! `src/internal/`. After the full `resolve_project` → `resolve_npm_dependencies`
//! → `bundle` pipeline runs, the helper's code must be inlined into the main
//! chunk — proving both that the alias resolves and that the bundler treats
//! `#internal/helper` as a local (bundled) import.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ngc_bundler::{bundle, BundleInput, BundleOptions};
use ngc_npm_resolver::package_json::DEVELOPMENT_BROWSER_CONDITIONS;
use ngc_npm_resolver::resolve_npm_dependencies;
use ngc_project_resolver::resolve_project;
use tempfile::tempdir;

#[test]
fn subpath_import_helper_is_inlined_into_main_chunk() {
    let temp = tempdir().expect("create temp dir");
    let root = temp.path();

    fs::write(
        root.join("tsconfig.json"),
        r#"{ "include": ["src/**/*.ts"], "exclude": [] }"#,
    )
    .expect("write tsconfig");

    fs::write(
        root.join("package.json"),
        r##"{
          "name": "subpath-fixture",
          "imports": {
            "#internal/*": "./src/internal/*.js"
          }
        }"##,
    )
    .expect("write package.json");

    let src = root.join("src");
    fs::create_dir_all(src.join("internal")).expect("create src/internal");

    fs::write(
        src.join("main.ts"),
        "import { helper } from '#internal/helper';\nconsole.log(helper());\n",
    )
    .expect("write main.ts");

    // Target of the `#internal/*` alias. The file is authored as `.js` to
    // match the mapping in package.json — the bundler picks it up as a
    // project file via the npm-resolver path, not the ts-transform path.
    fs::write(
        src.join("internal/helper.js"),
        "export const helper = () => 42;\n",
    )
    .expect("write helper.js");

    // Empty node_modules so the crawler doesn't early-return.
    fs::create_dir_all(root.join("node_modules")).expect("create node_modules");

    let file_graph = resolve_project(&root.join("tsconfig.json")).expect("resolve project");

    let entry = file_graph
        .entry_points
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "main.ts"))
        .cloned()
        .expect("main.ts should be an entry point");

    // Collect specifiers the project emits (including `#internal/helper`).
    let bare_specs: Vec<String> = file_graph.npm_import_sites.keys().cloned().collect();
    assert!(
        bare_specs.iter().any(|s| s == "#internal/helper"),
        "project scan should surface the `#internal/helper` specifier, got {bare_specs:?}"
    );

    let npm = resolve_npm_dependencies(&bare_specs, root, DEVELOPMENT_BROWSER_CONDITIONS)
        .expect("npm resolution");
    assert!(
        npm.resolved_specifiers.contains("#internal/helper"),
        "#internal/helper should resolve, got {:?}",
        npm.resolved_specifiers
    );

    // Merge project modules + npm-resolver-discovered helper into the graph.
    let mut graph = file_graph.graph;
    let mut path_index = file_graph.path_index;
    for path in npm.modules.keys() {
        if !path_index.contains_key(path) {
            let idx = graph.add_node(path.clone());
            path_index.insert(path.clone(), idx);
        }
    }

    // Add the edge: main.ts → internal/helper.js, keyed off the alias.
    let helper_path = npm
        .modules
        .keys()
        .find(|p| p.ends_with("src/internal/helper.js"))
        .cloned()
        .expect("helper.js should be in the npm resolution");
    for (spec, sites) in &file_graph.npm_import_sites {
        if spec == "#internal/helper" {
            let to_idx = path_index[&helper_path];
            for (from_file, kind) in sites {
                if let Some(&from_idx) = path_index.get(from_file) {
                    graph.add_edge(from_idx, to_idx, *kind);
                }
            }
        }
    }

    let mut modules: HashMap<PathBuf, String> = HashMap::new();
    for idx in graph.node_indices() {
        let path = &graph[idx];
        let source = npm
            .modules
            .get(path)
            .cloned()
            .or_else(|| fs::read_to_string(path).ok())
            .unwrap_or_else(|| panic!("source missing for {}", path.display()));
        modules.insert(path.clone(), source);
    }

    let input = BundleInput {
        modules,
        graph,
        entry,
        local_prefixes: vec![".".to_string()],
        root_dir: root.to_path_buf(),
        options: BundleOptions::default(),
        per_module_maps: HashMap::new(),
        bundled_specifiers: npm.resolved_specifiers.clone(),
        export_conditions: Vec::new(),
    };

    let output = bundle(&input).expect("bundle succeeds");

    let main_code = output
        .chunks
        .get(&output.main_filename)
        .expect("main chunk present");
    assert!(
        main_code.contains("helper"),
        "main chunk should inline the helper target: {main_code}"
    );
    assert!(
        !main_code.contains("'#internal/helper'") && !main_code.contains("\"#internal/helper\""),
        "main chunk must not leave the `#internal/helper` specifier in output: {main_code}"
    );
}
