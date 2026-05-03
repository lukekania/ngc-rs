//! Integration tests that exercise the `--output-json` payload of
//! `ngc-rs build`. The shape is consumed by `@ngc-rs/builder:application`,
//! so any drift in field names or types is a public-API break — these
//! tests pin the contract.
//!
//! Both the success and failure paths must emit a valid JSON object on
//! stdout: failure runs still set `success: false` and populate `errors`
//! before exiting non-zero, so the architect builder shim can parse a
//! coherent result without scraping stderr.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn write_minimal_fixture(root: &Path) {
    let tsconfig = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "preserve",
    "moduleResolution": "bundler",
    "outDir": "dist"
  },
  "include": ["src/**/*.ts"]
}"#;
    fs::write(root.join("tsconfig.json"), tsconfig).expect("write tsconfig");
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("main.ts"),
        "export const x: number = 1;\nconsole.log(x);\n",
    )
    .expect("write main.ts");
}

fn run_build_json(root: &Path, out_dir: &Path) -> (i32, String, String) {
    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let output = Command::new(bin)
        .args(["build", "--project"])
        .arg(root.join("tsconfig.json"))
        .arg("--out-dir")
        .arg(out_dir)
        .arg("--output-json")
        .output()
        .expect("spawn ngc-rs build");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code().unwrap_or(-1), stdout, stderr)
}

#[test]
fn output_json_success_payload_matches_builder_output_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_minimal_fixture(dir.path());
    let out_dir = dir.path().join("dist");

    let (code, stdout, stderr) = run_build_json(dir.path(), &out_dir);
    assert_eq!(code, 0, "build should succeed; stderr:\n{stderr}");

    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout is not valid JSON: {e}\n--- stdout ---\n{stdout}");
    });

    // Required fields and their types.
    assert_eq!(v["success"], Value::Bool(true));
    assert!(v["error"].is_null());
    assert!(v["errors"].is_array());
    assert_eq!(v["errors"].as_array().expect("errors").len(), 0);
    assert!(v["warnings"].is_array());
    assert!(v["output_path"].is_string());
    assert!(v["output_files"].is_array());
    assert!(v["modules_bundled"].is_number());
    assert!(v["total_size_bytes"].is_number());
    assert!(v["duration_ms"].is_number());

    // Output-files entries each have path/size/kind with the expected
    // kebab-case kind enum.
    let files = v["output_files"].as_array().expect("output_files");
    assert!(!files.is_empty(), "expected at least one output file");
    for f in files {
        assert!(f["path"].is_string(), "output file missing path: {f}");
        assert!(f["size"].is_number(), "output file missing size: {f}");
        let kind = f["kind"].as_str().expect("kind");
        assert!(
            matches!(kind, "script" | "style" | "html" | "source-map" | "asset"),
            "unexpected output kind: {kind}",
        );
    }
}

#[test]
fn output_json_failure_payload_emits_valid_json_with_success_false() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Intentionally invalid: tsconfig missing on the path we point at.
    let out_dir = dir.path().join("dist");
    let bogus_tsconfig = dir.path().join("does-not-exist.json");

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let output = Command::new(bin)
        .args(["build", "--project"])
        .arg(&bogus_tsconfig)
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--output-json")
        .output()
        .expect("spawn ngc-rs build");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert_ne!(
        output.status.code(),
        Some(0),
        "build should fail when tsconfig is missing",
    );

    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("failure path must still emit valid JSON; parse error: {e}\nstdout:\n{stdout}");
    });

    assert_eq!(v["success"], Value::Bool(false));
    assert!(
        v["error"].is_string(),
        "failure payload should populate `error`: {v}"
    );
    assert!(
        v["errors"].is_array() && !v["errors"].as_array().unwrap().is_empty(),
        "failure payload should populate `errors`: {v}"
    );
    let first = &v["errors"][0];
    assert!(first["message"].is_string());
    assert_eq!(first["severity"], Value::String("error".to_string()));
}
