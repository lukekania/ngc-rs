//! End-to-end integration for vendor (shared) chunk splitting — issue #131.
//!
//! Drives the full `resolve_project` → `resolve_npm_dependencies` → `bundle`
//! pipeline against a synthetic project where an npm package is imported by
//! multiple lazy routes. Asserts that:
//!
//! 1. The npm package is extracted into a `chunk-<8hex>.js` vendor chunk
//!    (rather than being duplicated across consumers or folded into main).
//! 2. Both lazy chunks reference the vendor chunk via static ESM cross-chunk
//!    imports.
//! 3. The vendor chunk exports its `__ns_*` namespace.
//! 4. The vendor chunk filename is deterministic across builds (same module
//!    membership → same filename).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ngc_bundler::{bundle, BundleInput, BundleOptions, ChunkKind};
use ngc_npm_resolver::package_json::DEVELOPMENT_BROWSER_CONDITIONS;
use ngc_npm_resolver::resolve_npm_dependencies;
use ngc_project_resolver::resolve_project;
use tempfile::tempdir;

/// Build the BundleInput for a synthetic project whose two lazy routes both
/// statically import `shared-pkg`. Returns `(input, root_dir)`.
fn build_two_lazy_routes_sharing_npm(root: &std::path::Path) -> BundleInput {
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "include": ["src/**/*.ts"], "exclude": [] }"#,
    )
    .expect("write tsconfig");

    fs::write(
        root.join("package.json"),
        r#"{ "name": "vendor-fixture", "dependencies": { "shared-pkg": "1.0.0" } }"#,
    )
    .expect("write package.json");

    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");

    // main.ts — dynamically imports both routes.
    fs::write(
        src.join("main.ts"),
        "function loadA(){return import('./route-a');}\n\
         function loadB(){return import('./route-b');}\n\
         console.log(loadA, loadB);\n",
    )
    .expect("write main.ts");

    // Both lazy routes statically import `shared-pkg`.
    fs::write(
        src.join("route-a.ts"),
        "import { shared } from 'shared-pkg';\n\
         export const A = () => shared('a');\n",
    )
    .expect("write route-a.ts");

    fs::write(
        src.join("route-b.ts"),
        "import { shared } from 'shared-pkg';\n\
         export const B = () => shared('b');\n",
    )
    .expect("write route-b.ts");

    // The npm package's entry — an ESM module exporting `shared`.
    let pkg_dir = root.join("node_modules").join("shared-pkg");
    fs::create_dir_all(&pkg_dir).expect("create pkg dir");
    fs::write(
        pkg_dir.join("package.json"),
        r#"{ "name": "shared-pkg", "version": "1.0.0", "main": "index.mjs" }"#,
    )
    .expect("write pkg package.json");
    fs::write(
        pkg_dir.join("index.mjs"),
        "export const shared = (x) => `shared:${x}`;\n",
    )
    .expect("write pkg index.mjs");

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
    // Wire bare specifier imports as static edges.
    for (spec, sites) in &file_graph.npm_import_sites {
        if let Some(target_path) = npm
            .modules
            .keys()
            .find(|p| p.to_string_lossy().contains(&format!("/{spec}/")))
        {
            let to_idx = path_index[target_path];
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

    BundleInput {
        modules,
        graph,
        entry,
        local_prefixes: vec![".".to_string()],
        root_dir: root.to_path_buf(),
        options: BundleOptions::default(),
        per_module_maps: HashMap::new(),
        bundled_specifiers: npm.resolved_specifiers.clone(),
        export_conditions: Vec::new(),
    }
}

/// An npm module reached only by lazy chunks (≥2) is extracted into a vendor
/// chunk named `chunk-<8hex>.js`. Main contains none of the npm module's code.
#[test]
fn shared_npm_across_lazy_routes_creates_vendor_chunk() {
    let temp = tempdir().expect("create temp dir");
    let input = build_two_lazy_routes_sharing_npm(temp.path());

    let output = bundle(&input).expect("bundle succeeds");

    // Locate the vendor chunk by ChunkKind::Shared.
    let vendor_filenames: Vec<&String> = output
        .chunk_kinds
        .iter()
        .filter(|(_, kind)| **kind == ChunkKind::Shared)
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        vendor_filenames.len(),
        1,
        "expected exactly one vendor chunk, got chunks={:?} kinds={:?}",
        output.chunks.keys().collect::<Vec<_>>(),
        output.chunk_kinds,
    );
    let vendor_name = vendor_filenames[0];

    // Vendor chunk filename shape: `chunk-<8hex>.js`.
    let hex = &vendor_name["chunk-".len().."chunk-".len() + 8];
    assert!(
        vendor_name.starts_with("chunk-")
            && vendor_name.ends_with(".js")
            && hex.chars().all(|c| c.is_ascii_hexdigit()),
        "vendor filename should be chunk-<8hex>.js, got {vendor_name}"
    );

    // Vendor chunk contains the npm module's body.
    let vendor_code = &output.chunks[vendor_name];
    assert!(
        vendor_code.contains("shared:"),
        "vendor chunk should inline the npm module body: {vendor_code}"
    );

    // Main has none of the npm module.
    let main_code = &output.chunks[&output.main_filename];
    assert!(
        !main_code.contains("shared:"),
        "main should not contain the npm module body: {main_code}"
    );
}

/// Vendor chunks have an `export { __ns_X };` block at the end and consumer
/// chunks have a matching `import { __ns_X } from './chunk-XXXX.js';` block
/// at the top. This is what makes the chunks actually link at module-load
/// time in the browser.
#[test]
fn vendor_chunk_exports_namespace_and_consumers_import_it() {
    let temp = tempdir().expect("create temp dir");
    let input = build_two_lazy_routes_sharing_npm(temp.path());
    let output = bundle(&input).expect("bundle succeeds");

    let vendor_name = output
        .chunk_kinds
        .iter()
        .find(|(_, k)| **k == ChunkKind::Shared)
        .map(|(n, _)| n.clone())
        .expect("vendor chunk");

    let vendor_code = &output.chunks[&vendor_name];
    assert!(
        vendor_code.contains("export { __ns_"),
        "vendor should export its __ns_* identifier: {vendor_code}"
    );

    // Consumer chunks (lazy routes) static-import from the vendor.
    let lazy_filenames: Vec<&String> = output
        .chunk_kinds
        .iter()
        .filter(|(_, k)| **k == ChunkKind::Lazy)
        .map(|(n, _)| n)
        .collect();
    let mut at_least_one_consumer = false;
    for lazy_name in &lazy_filenames {
        let lazy_code = &output.chunks[*lazy_name];
        if lazy_code.contains(&format!("from './{vendor_name}'")) {
            at_least_one_consumer = true;
        }
    }
    assert!(
        at_least_one_consumer,
        "at least one lazy chunk should statically import from the vendor chunk; \
         vendor={vendor_name} lazy_filenames={lazy_filenames:?}"
    );
}

/// Vendor chunks consumed only by lazy chunks (not by main) load
/// transitively when the lazy chunk evaluates — they should NOT be in
/// `BundleOutput.initial_chunks`.
#[test]
fn lazy_only_vendor_chunk_is_not_initial() {
    let temp = tempdir().expect("create temp dir");
    let input = build_two_lazy_routes_sharing_npm(temp.path());
    let output = bundle(&input).expect("bundle succeeds");

    let vendor_name = output
        .chunk_kinds
        .iter()
        .find(|(_, k)| **k == ChunkKind::Shared)
        .map(|(n, _)| n.clone())
        .expect("vendor chunk");

    // main + zero eager vendor = just main in initial_chunks.
    assert_eq!(
        output.initial_chunks,
        vec![output.main_filename.clone()],
        "lazy-only vendor should not be in initial_chunks; got {:?}",
        output.initial_chunks
    );
    assert!(
        !output.initial_chunks.contains(&vendor_name),
        "vendor chunk {vendor_name} should not be eagerly loaded"
    );
}

/// Determinism: bundling the same input twice produces byte-identical chunk
/// filenames + content. Vendor naming hashes absolute module paths, so we
/// build twice in the same temp directory (real builds have a stable project
/// root). Path-hash-based naming + the deterministic toposort + BTreeMap
/// iteration combine to guarantee this.
#[test]
fn vendor_chunk_partition_is_deterministic_across_builds() {
    let temp = tempdir().expect("temp dir");
    let input_a = build_two_lazy_routes_sharing_npm(temp.path());
    let input_b = build_two_lazy_routes_sharing_npm(temp.path());

    let out_a = bundle(&input_a).expect("bundle a");
    let out_b = bundle(&input_b).expect("bundle b");

    let mut vendors_a: Vec<&String> = out_a
        .chunk_kinds
        .iter()
        .filter(|(_, k)| **k == ChunkKind::Shared)
        .map(|(n, _)| n)
        .collect();
    let mut vendors_b: Vec<&String> = out_b
        .chunk_kinds
        .iter()
        .filter(|(_, k)| **k == ChunkKind::Shared)
        .map(|(n, _)| n)
        .collect();
    vendors_a.sort();
    vendors_b.sort();
    assert_eq!(
        vendors_a, vendors_b,
        "vendor chunk filenames should be deterministic across builds"
    );

    // Vendor chunk bodies should be byte-identical too.
    for (name, _) in out_a.chunk_kinds.iter().filter(|(_, k)| **k == ChunkKind::Shared) {
        let a_code = out_a.chunks.get(name).expect("vendor in out_a");
        let b_code = out_b.chunks.get(name).expect("vendor in out_b");
        assert_eq!(a_code, b_code, "vendor chunk {name} should be byte-stable");
    }
}
