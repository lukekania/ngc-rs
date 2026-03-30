use std::collections::HashMap;
use std::path::PathBuf;

use ngc_bundler::{BundleInput, BundleOutput};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/simple-app")
}

fn build_bundle() -> BundleOutput {
    let tsconfig_path = fixture_path().join("tsconfig.app.json");
    let config = ngc_project_resolver::tsconfig::resolve_tsconfig(&tsconfig_path)
        .expect("should resolve tsconfig");
    let file_graph =
        ngc_project_resolver::resolve_project(&tsconfig_path).expect("should resolve project");

    let config_dir = config.config_path.parent().unwrap().to_path_buf();

    let root_dir = config
        .compiler_options
        .root_dir
        .as_ref()
        .map(|r| config_dir.join(r))
        .unwrap_or_else(|| config_dir.clone());
    let root_dir = root_dir.canonicalize().expect("root_dir should exist");

    let files: Vec<PathBuf> = file_graph.graph.node_weights().cloned().collect();

    // Compile templates, then transform to JS (mirrors the full CLI pipeline)
    let compiled =
        ngc_template_compiler::compile_templates(&files).expect("should compile templates");
    let sources: Vec<(PathBuf, String)> = compiled
        .into_iter()
        .map(|cf| (cf.source_path, cf.source))
        .collect();
    let transformed =
        ngc_ts_transform::transform_sources_to_memory(&sources).expect("should transform files");

    let modules: HashMap<PathBuf, String> = transformed
        .into_iter()
        .map(|m| (m.source_path, m.code))
        .collect();

    let entry = file_graph
        .entry_points
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "main.ts"))
        .expect("should find main.ts entry point")
        .clone();

    let mut local_prefixes = vec![".".to_string()];
    if let Some(paths) = &config.compiler_options.paths {
        for alias in paths.keys() {
            if let Some(prefix) = alias.strip_suffix('*') {
                local_prefixes.push(prefix.to_string());
            }
        }
    }

    let input = BundleInput {
        modules,
        graph: file_graph.graph,
        entry,
        local_prefixes,
        root_dir,
        options: ngc_bundler::BundleOptions::default(),
    };

    ngc_bundler::bundle(&input).expect("should bundle")
}

/// Helper to get the main chunk code from a BundleOutput.
fn main_chunk(output: &BundleOutput) -> &str {
    output
        .chunks
        .get(&output.main_filename)
        .expect("main chunk should exist")
}

#[test]
fn test_bundle_snapshot() {
    let output = build_bundle();
    let bundle = main_chunk(&output);
    insta::assert_snapshot!("bundle_output", bundle);
}

#[test]
fn test_bundle_has_no_local_imports() {
    let output = build_bundle();
    let bundle = main_chunk(&output);
    for line in bundle.lines() {
        if line.starts_with("import") && line.contains("from") {
            let from_part = line.split("from").last().unwrap_or("");
            assert!(
                !from_part.contains("./"),
                "bundle should not contain relative imports: {line}"
            );
            assert!(
                !from_part.contains("@app/"),
                "bundle should not contain @app/ alias imports: {line}"
            );
            assert!(
                !from_part.contains("@env/"),
                "bundle should not contain @env/ alias imports: {line}"
            );
        }
    }
}

#[test]
fn test_bundle_preserves_external_imports() {
    let output = build_bundle();
    let bundle = main_chunk(&output);
    assert!(
        bundle.contains("@angular/core"),
        "should preserve @angular/core import"
    );
    assert!(
        bundle.contains("@angular/router"),
        "should preserve @angular/router import"
    );
    assert!(
        bundle.contains("@angular/platform-browser"),
        "should preserve @angular/platform-browser import"
    );
}

#[test]
fn test_bundle_excludes_unreachable_modules() {
    let output = build_bundle();
    let bundle = main_chunk(&output);
    // environment.prod.ts exports `production: true` and is not reachable from main.ts
    assert!(
        !bundle.contains("environment.prod"),
        "unreachable environment.prod should not be in bundle"
    );
    // But environment.ts (production: false) IS reachable
    assert!(
        bundle.contains("production: false"),
        "reachable environment should be in bundle"
    );
}

#[test]
fn test_bundle_entry_point_last() {
    let output = build_bundle();
    let bundle = main_chunk(&output);
    let main_comment_pos = bundle
        .rfind("// src/main.js")
        .expect("main.js comment should exist");
    // Every other module comment should appear before main
    for line in bundle[..main_comment_pos].lines() {
        if line.starts_with("// src/") {
            // This is fine — other modules are before main
        }
    }
    // No module comments after main
    let after_main = &bundle[main_comment_pos + 14..];
    assert!(
        !after_main.contains("\n// src/"),
        "no module should appear after the entry point"
    );
}
