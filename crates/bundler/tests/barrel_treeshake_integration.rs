//! End-to-end tree-shaking of a barrel-export npm package — issue #171.
//!
//! Reproduces the real lodash-es shape that a per-provider vendor shake alone
//! did not handle:
//!
//! - A lazy route imports a SINGLE named export via a BARE specifier
//!   (`import { debounce } from 'barrel-pkg'`).
//! - The package entry is a pure re-export barrel
//!   (`export { default as debounce } from './debounce.js'`, one line per
//!   method) that ALSO re-exports a `default` aggregator.
//! - The `default` aggregator (`lodash.default.js`-style) imports every method
//!   and has hundreds of top-level assignment statements — i.e. it looks
//!   side-effectful — but the package declares `"sideEffects": false`.
//!
//! Correct output: only `debounce` and its transitive deps survive; the unused
//! `throttle` leaf and the side-effect-free-but-unreached aggregator are
//! dropped. Before the fix the aggregator's top-level statements pinned it as
//! side-effectful, dragging the entire package into the chunk.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ngc_bundler::{bundle, BundleInput, BundleOptions};
use ngc_npm_resolver::package_json::DEVELOPMENT_BROWSER_CONDITIONS;
use ngc_npm_resolver::resolve_npm_dependencies;
use ngc_project_resolver::resolve_project;
use tempfile::tempdir;

fn build_barrel_project(root: &std::path::Path) -> BundleInput {
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "include": ["src/**/*.ts"], "exclude": [] }"#,
    )
    .expect("write tsconfig");
    fs::write(
        root.join("package.json"),
        r#"{ "name": "barrel-fixture", "dependencies": { "barrel-pkg": "1.0.0" } }"#,
    )
    .expect("write package.json");

    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");

    // main.ts lazily loads the route so it becomes its own chunk.
    fs::write(
        src.join("main.ts"),
        "function load(){return import('./route');}\nconsole.log(load);\n",
    )
    .expect("write main.ts");

    // The route imports ONE method via the bare package specifier.
    fs::write(
        src.join("route.ts"),
        "import { debounce } from 'barrel-pkg';\nexport const R = () => debounce(() => {});\n",
    )
    .expect("write route.ts");

    // barrel-pkg: sideEffects:false, entry is a pure re-export barrel.
    let pkg_dir = root.join("node_modules").join("barrel-pkg");
    fs::create_dir_all(&pkg_dir).expect("create pkg dir");
    fs::write(
        pkg_dir.join("package.json"),
        r#"{ "name": "barrel-pkg", "version": "1.0.0", "module": "barrel.js", "main": "barrel.js", "sideEffects": false }"#,
    )
    .expect("write pkg package.json");
    fs::write(
        pkg_dir.join("barrel.js"),
        "export { default as debounce } from './debounce.js';\n\
         export { default as throttle } from './throttle.js';\n\
         export { default } from './aggregator.js';\n",
    )
    .expect("write barrel.js");
    // debounce depends on a shared helper.
    fs::write(
        pkg_dir.join("debounce.js"),
        "import helper from './helper.js';\n\
         function debounce(fn){ return helper(fn); }\n\
         export default debounce;\n",
    )
    .expect("write debounce.js");
    fs::write(
        pkg_dir.join("throttle.js"),
        "function throttle(fn){ return fn; }\nexport default throttle;\n",
    )
    .expect("write throttle.js");
    fs::write(
        pkg_dir.join("helper.js"),
        "function helper(fn){ return fn; }\nexport default helper;\n",
    )
    .expect("write helper.js");
    // The aggregator LOOKS side-effectful (top-level statements) and imports
    // every method — but sideEffects:false means it can be dropped when unused.
    fs::write(
        pkg_dir.join("aggregator.js"),
        "import debounce from './debounce.js';\n\
         import throttle from './throttle.js';\n\
         var _ = {};\n\
         _.debounce = debounce;\n\
         _.throttle = throttle;\n\
         export default _;\n",
    )
    .expect("write aggregator.js");

    let file_graph = resolve_project(&root.join("tsconfig.json")).expect("resolve project");
    let entry = file_graph
        .entry_points
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "main.ts"))
        .cloned()
        .expect("main.ts should be an entry point");

    let bare_specs: Vec<String> = file_graph.npm_import_sites.keys().cloned().collect();
    let npm = resolve_npm_dependencies(&bare_specs, root, DEVELOPMENT_BROWSER_CONDITIONS)
        .expect("npm resolution");

    let mut graph = file_graph.graph;
    let mut path_index = file_graph.path_index;
    for path in npm.modules.keys() {
        if !path_index.contains_key(path) {
            let idx = graph.add_node(path.clone());
            path_index.insert(path.clone(), idx);
        }
    }
    // Bare specifier edges: route.ts -> barrel.js entry.
    for (spec, sites) in &file_graph.npm_import_sites {
        if let Some(target_path) = npm
            .modules
            .keys()
            .find(|p| p.to_string_lossy().contains(&format!("/{spec}/barrel.js")))
        {
            let to_idx = path_index[target_path];
            for (from_file, kind) in sites {
                if let Some(&from_idx) = path_index.get(from_file) {
                    graph.add_edge(from_idx, to_idx, *kind);
                }
            }
        }
    }
    // Internal npm edges (barrel -> leaves, aggregator -> leaves).
    for (from, to, kind) in &npm.edges {
        if let (Some(&f), Some(&t)) = (path_index.get(from), path_index.get(to)) {
            graph.add_edge(f, t, *kind);
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

    BundleInput {
        modules,
        graph,
        entry,
        local_prefixes: vec![".".to_string()],
        root_dir: root.to_path_buf(),
        options: BundleOptions {
            tree_shake: true,
            ..BundleOptions::default()
        },
        per_module_maps: HashMap::new(),
        bundled_specifiers: npm.resolved_specifiers.clone(),
        external_specifiers: Default::default(),
        export_conditions: Vec::new(),
    }
}

#[test]
fn barrel_pkg_shakes_to_used_export_only() {
    let temp = tempdir().expect("create temp dir");
    let input = build_barrel_project(temp.path());

    let output = bundle(&input).expect("bundle succeeds");

    // The lazy route chunk carries the used method and its helper...
    let all_code: String = output
        .chunks
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_code.contains("function debounce"),
        "used export `debounce` must survive"
    );
    assert!(
        all_code.contains("function helper"),
        "debounce's transitive dep `helper` must survive"
    );
    // ...but NOT the unused leaf, nor the side-effect-free-but-unreached
    // aggregator that would otherwise drag the whole package in.
    assert!(
        !all_code.contains("function throttle"),
        "unused leaf `throttle` must be tree-shaken out:\n{all_code}"
    );
    assert!(
        !all_code.contains("_.throttle = throttle"),
        "unreached `sideEffects:false` aggregator must be dropped:\n{all_code}"
    );
}
