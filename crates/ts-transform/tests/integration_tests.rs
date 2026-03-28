use std::path::PathBuf;
use tempfile::TempDir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/simple-app")
}

#[test]
fn test_transform_project_produces_js_files() {
    let fixture = fixture_root().join("tsconfig.app.json");
    let graph = ngc_project_resolver::resolve_project(&fixture).expect("fixture should resolve");

    let out_dir = TempDir::new().unwrap();
    let root_dir = fixture_root().join("src").canonicalize().unwrap();
    let files: Vec<PathBuf> = graph.graph.node_weights().cloned().collect();

    let result = ngc_ts_transform::transform_project(&files, &root_dir, out_dir.path())
        .expect("transform should succeed");

    assert_eq!(result.files_transformed, 9);
    assert!(out_dir.path().join("app/app.component.js").exists());
    assert!(out_dir.path().join("main.js").exists());
    assert!(out_dir.path().join("app/shared/logger.js").exists());
    assert!(out_dir.path().join("environments/environment.js").exists());
}

#[test]
fn test_output_has_no_type_annotations() {
    let source = r#"
import { Component } from '@angular/core';

interface Config {
    name: string;
    value: number;
}

export function greet(name: string): string {
    return `Hello, ${name}`;
}
"#;
    let result =
        ngc_ts_transform::transform_source(source, "test.ts").expect("transform should succeed");

    assert!(!result.contains(": string"));
    assert!(!result.contains(": number"));
    assert!(!result.contains("interface Config"));
    assert!(result.contains("function greet"));
}

#[test]
fn test_decorators_are_transformed() {
    let source = r#"
function Component(config: any) { return (target: any) => target; }

@Component({
    selector: 'app-root',
    template: '<h1>Hello</h1>'
})
export class AppComponent {
    title = 'app';
}
"#;
    let result =
        ngc_ts_transform::transform_source(source, "test.ts").expect("transform should succeed");

    assert!(
        !result.contains("@Component"),
        "decorator syntax should be transformed"
    );
    assert!(
        result.contains("class AppComponent"),
        "class should be preserved"
    );
}

#[test]
fn test_output_preserves_directory_structure() {
    let fixture = fixture_root().join("tsconfig.app.json");
    let graph = ngc_project_resolver::resolve_project(&fixture).expect("fixture should resolve");

    let out_dir = TempDir::new().unwrap();
    let root_dir = fixture_root().join("src").canonicalize().unwrap();
    let files: Vec<PathBuf> = graph.graph.node_weights().cloned().collect();

    ngc_ts_transform::transform_project(&files, &root_dir, out_dir.path())
        .expect("transform should succeed");

    // Verify nested directory structure is preserved
    assert!(out_dir.path().join("app").is_dir());
    assert!(out_dir.path().join("app/shared").is_dir());
    assert!(out_dir.path().join("environments").is_dir());

    // Verify no .ts files in output
    let has_ts_files = walkdir(out_dir.path())
        .iter()
        .any(|p| p.extension().is_some_and(|e| e == "ts"));
    assert!(!has_ts_files, "output should contain no .ts files");
}

/// Recursively collect all files under a directory.
fn walkdir(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}
