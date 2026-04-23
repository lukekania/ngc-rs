//! End-to-end integration tests for component style preprocessing.
//!
//! Exercises the `compile_all_decorators_with_styles` pipeline on synthetic
//! `@Component` fixtures that use:
//!   - `.scss` / `.sass` / `.less` / `.styl` `styleUrl`/`styleUrls`
//!   - `styles: [\`...\`]` with `inlineStyleLanguage: scss`
//!   - missing-npm-package diagnostics
//!
//! These tests shell out to real Node subprocesses via the `sass`, `less`,
//! and `stylus` npm packages. Packages are installed once per test run into a
//! per-user cache dir under the system temp directory; individual test
//! fixtures symlink their `node_modules` to this cache. Tests that cannot
//! locate a `node` binary or `npm` are skipped with a log line — the missing-
//! package diagnostic test does not need them and always runs.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use ngc_template_compiler::{
    compile_all_decorators_with_styles, compile_component_with_styles, StyleContext, StyleLanguage,
};
use tempfile::tempdir;

/// Install `sass`, `less`, and `stylus` into a shared cache directory so each
/// test can symlink its `node_modules` to the cache. Returns the absolute
/// path to `node_modules` on success, or `None` if node/npm are unavailable.
fn preprocessor_cache() -> Option<&'static Path> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if !command_exists("node") || !command_exists("npm") {
                eprintln!("ngc-preprocessor-integration: node/npm not on PATH — skipping install");
                return None;
            }
            let cache_root = std::env::temp_dir().join("ngc-preprocessor-test-cache-v1");
            let node_modules = cache_root.join("node_modules");

            let all_present = ["sass", "less", "stylus"]
                .iter()
                .all(|pkg| node_modules.join(pkg).is_dir());
            if all_present {
                return Some(node_modules);
            }
            std::fs::create_dir_all(&cache_root).ok()?;
            // package.json so npm treats the cache root as a project.
            std::fs::write(
                cache_root.join("package.json"),
                "{\"name\":\"ngc-preprocessor-cache\",\"private\":true}",
            )
            .ok()?;

            eprintln!(
                "ngc-preprocessor-integration: installing sass, less, stylus into {}",
                cache_root.display()
            );
            let status = Command::new("npm")
                .arg("install")
                .arg("--no-audit")
                .arg("--no-fund")
                .arg("--silent")
                .arg("sass@1.77.8")
                .arg("less@4.2.0")
                .arg("stylus@0.64.0")
                .current_dir(&cache_root)
                .status();
            match status {
                Ok(s) if s.success() => Some(node_modules),
                Ok(s) => {
                    eprintln!("ngc-preprocessor-integration: npm install exited with {s}");
                    None
                }
                Err(e) => {
                    eprintln!("ngc-preprocessor-integration: could not run npm: {e}");
                    None
                }
            }
        })
        .as_deref()
}

fn command_exists(cmd: &str) -> bool {
    which::which(cmd).is_ok()
}

/// Build a fresh temp project rooted at a new directory with its
/// `node_modules` symlinked to the shared cache. Returns `None` when the
/// cache is unavailable (tests should skip in that case).
fn make_project_with_node_modules() -> Option<(tempfile::TempDir, PathBuf)> {
    let cache = preprocessor_cache()?;
    let dir = tempdir().expect("create tempdir");
    let project_root = dir.path().to_path_buf();
    symlink(cache, project_root.join("node_modules")).expect("symlink node_modules");
    Some((dir, project_root))
}

fn component_source(imports: &str, decorator_body: &str) -> String {
    format!(
        "import {{ Component }} from '@angular/core';\n\
         {imports}\n\
         @Component({{\n\
             selector: 'app-test',\n\
             standalone: true,\n\
             template: '<div class=\"box\">hi</div>',\n\
             {decorator_body}\n\
         }})\n\
         export class TestComponent {{}}\n"
    )
}

// ---------------------------------------------------------------------------
// Diagnostics: missing package
// ---------------------------------------------------------------------------

#[test]
fn missing_sass_package_emits_clear_style_error() {
    let dir = tempdir().expect("create tempdir");
    let project_root = dir.path().to_path_buf();
    // Intentionally no node_modules.
    let component_path = project_root.join("app.component.ts");
    std::fs::write(
        project_root.join("styles.scss"),
        "$c: red;\n.box { color: $c; }\n",
    )
    .unwrap();
    std::fs::write(
        &component_path,
        component_source("", "styleUrl: './styles.scss'"),
    )
    .unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Css,
    };
    let err = compile_component_with_styles(
        &std::fs::read_to_string(&component_path).unwrap(),
        &component_path,
        &ctx,
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("`sass` npm package is not installed"),
        "expected missing-package diagnostic, got: {msg}"
    );
    assert!(
        msg.contains("npm install --save-dev sass"),
        "expected install hint, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// styleUrl: './foo.scss'
// ---------------------------------------------------------------------------

#[test]
fn scss_style_url_is_compiled_and_inlined_into_define_component() {
    let Some((_guard, project_root)) = make_project_with_node_modules() else {
        return;
    };
    let component_path = project_root.join("widget.component.ts");
    std::fs::write(
        project_root.join("widget.component.scss"),
        "$brand: #abcdef;\n\
         .box {\n\
             color: $brand;\n\
             &__inner { padding: 4px; }\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        &component_path,
        component_source("", "styleUrl: './widget.component.scss'"),
    )
    .unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Css,
    };
    let result = compile_component_with_styles(
        &std::fs::read_to_string(&component_path).unwrap(),
        &component_path,
        &ctx,
    )
    .expect("compile with scss styleUrl");
    assert!(result.compiled, "component should compile");

    let out = &result.source;
    assert!(
        out.contains("styles:"),
        "defineComponent should carry a styles array: {out}"
    );
    assert!(
        out.contains("#abcdef"),
        "SCSS variable should resolve to its concrete color: {out}"
    );
    assert!(
        out.contains(".box__inner"),
        "SCSS nested selector should be flattened: {out}"
    );
    assert!(
        !out.contains("$brand"),
        "raw SCSS source must not appear in output: {out}"
    );
}

#[test]
fn style_urls_array_preserves_order_and_compiles_each_language() {
    let Some((_guard, project_root)) = make_project_with_node_modules() else {
        return;
    };
    let component_path = project_root.join("mixed.component.ts");
    std::fs::write(
        project_root.join("first.scss"),
        ".first { color: #111111; }\n",
    )
    .unwrap();
    std::fs::write(
        project_root.join("second.css"),
        ".second { color: #222222; }\n",
    )
    .unwrap();
    std::fs::write(
        &component_path,
        component_source("", "styleUrls: ['./first.scss', './second.css']"),
    )
    .unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Css,
    };
    let result = compile_component_with_styles(
        &std::fs::read_to_string(&component_path).unwrap(),
        &component_path,
        &ctx,
    )
    .expect("compile with styleUrls array");
    assert!(result.compiled);
    let out = &result.source;
    let first_pos = out.find("#111111").expect("first.scss compiled color");
    let second_pos = out.find("#222222").expect("second.css inlined color");
    assert!(
        first_pos < second_pos,
        "styleUrls array order must be preserved in styles output"
    );
}

// ---------------------------------------------------------------------------
// inlineStyleLanguage: scss → inline styles[] preprocessed
// ---------------------------------------------------------------------------

#[test]
fn inline_style_language_scss_preprocesses_template_literal_bodies() {
    let Some((_guard, project_root)) = make_project_with_node_modules() else {
        return;
    };
    let component_path = project_root.join("theme.component.ts");
    let body = "styles: [`\n\
        $radius: 8px;\n\
        .box { border-radius: $radius; &:hover { opacity: 0.5; } }\n\
    `]";
    std::fs::write(&component_path, component_source("", body)).unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Scss,
    };
    let result = compile_component_with_styles(
        &std::fs::read_to_string(&component_path).unwrap(),
        &component_path,
        &ctx,
    )
    .expect("compile with inlineStyleLanguage scss");
    assert!(result.compiled);
    let out = &result.source;
    assert!(
        out.contains("8px"),
        "SCSS variable should resolve to its value: {out}"
    );
    assert!(
        out.contains(".box:hover"),
        "nested `&:hover` should be flattened: {out}"
    );
    assert!(
        !out.contains("$radius"),
        "raw SCSS variable must not appear after preprocessing: {out}"
    );
}

// ---------------------------------------------------------------------------
// .less and .styl
// ---------------------------------------------------------------------------

#[test]
fn less_style_url_is_compiled() {
    let Some((_guard, project_root)) = make_project_with_node_modules() else {
        return;
    };
    let component_path = project_root.join("less.component.ts");
    std::fs::write(
        project_root.join("less.component.less"),
        "@brand: #112233;\n\
         .box { color: @brand; .inner { padding: 4px; } }\n",
    )
    .unwrap();
    std::fs::write(
        &component_path,
        component_source("", "styleUrl: './less.component.less'"),
    )
    .unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Css,
    };
    let result = compile_component_with_styles(
        &std::fs::read_to_string(&component_path).unwrap(),
        &component_path,
        &ctx,
    )
    .expect("compile with less styleUrl");
    assert!(result.compiled);
    let out = &result.source;
    assert!(out.contains("#112233"), "less @variable resolved: {out}");
    assert!(
        out.contains(".box .inner"),
        "less nested selector flattened: {out}"
    );
}

#[test]
fn stylus_style_url_is_compiled() {
    let Some((_guard, project_root)) = make_project_with_node_modules() else {
        return;
    };
    let component_path = project_root.join("styl.component.ts");
    std::fs::write(
        project_root.join("styl.component.styl"),
        "brand = #789abc\n.box\n  color brand\n",
    )
    .unwrap();
    std::fs::write(
        &component_path,
        component_source("", "styleUrl: './styl.component.styl'"),
    )
    .unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Css,
    };
    let result = compile_component_with_styles(
        &std::fs::read_to_string(&component_path).unwrap(),
        &component_path,
        &ctx,
    )
    .expect("compile with stylus styleUrl");
    assert!(result.compiled);
    let out = &result.source;
    assert!(out.contains("#789abc"), "stylus variable resolved: {out}");
    assert!(
        out.to_ascii_lowercase().contains(".box"),
        "stylus selector emitted: {out}"
    );
}

// ---------------------------------------------------------------------------
// Batch driver: the parallel `compile_all_decorators_with_styles` path runs
// the same preprocessor and returns compiled files.
// ---------------------------------------------------------------------------

#[test]
fn compile_all_decorators_processes_scss_component_in_parallel_path() {
    let Some((_guard, project_root)) = make_project_with_node_modules() else {
        return;
    };
    let component_path = project_root.join("parallel.component.ts");
    std::fs::write(
        project_root.join("parallel.scss"),
        "$c: #9988aa;\n.parallel { color: $c; }\n",
    )
    .unwrap();
    std::fs::write(
        &component_path,
        component_source("", "styleUrl: './parallel.scss'"),
    )
    .unwrap();

    let ctx = StyleContext {
        project_root: project_root.clone(),
        inline_style_language: StyleLanguage::Css,
    };
    let results = compile_all_decorators_with_styles(std::slice::from_ref(&component_path), &ctx)
        .expect("compile_all should succeed");
    assert_eq!(results.len(), 1);
    let cf = &results[0];
    assert!(cf.compiled);
    assert!(
        cf.source.contains("#9988aa"),
        "compile_all pipeline must run preprocessor: {}",
        cf.source
    );
}
