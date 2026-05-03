//! End-to-end test that the `budgets` array in angular.json is parsed,
//! evaluated against emitted output, and reflected in both the human
//! summary (exit code) and the `--output-json` payload (`success`,
//! `errors`, `warnings`).
//!
//! Uses an unrealistically tight `initial` budget (1 byte) so any
//! non-trivial Angular build hits the threshold deterministically.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn write_fixture(root: &Path, budgets_json: &str) {
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

    let angular_json = format!(
        r#"{{
  "$schema": "./node_modules/@angular/cli/lib/config/schema.json",
  "version": 1,
  "newProjectRoot": "projects",
  "projects": {{
    "demo": {{
      "projectType": "application",
      "root": "",
      "sourceRoot": "src",
      "architect": {{
        "build": {{
          "builder": "@angular/build:application",
          "options": {{
            "tsConfig": "tsconfig.json"
          }},
          "configurations": {{
            "production": {{
              "budgets": {budgets}
            }}
          }}
        }}
      }}
    }}
  }}
}}
"#,
        budgets = budgets_json
    );
    fs::write(root.join("angular.json"), angular_json).expect("write angular.json");
}

#[test]
fn over_budget_production_build_fails_and_emits_json_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 1-byte budget — guaranteed to fail for any non-empty bundle.
    write_fixture(dir.path(), r#"[{"type": "initial", "maximumError": 1}]"#);

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out_dir = dir.path().join("dist");
    let output = Command::new(bin)
        .args(["build", "--project"])
        .arg(dir.path().join("tsconfig.json"))
        .arg("--out-dir")
        .arg(&out_dir)
        .args(["-c", "production", "--output-json"])
        .output()
        .expect("spawn ngc-rs build");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert_ne!(
        output.status.code(),
        Some(0),
        "build should fail with a budget error",
    );

    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("failure path must emit valid JSON; parse error: {e}\nstdout:\n{stdout}");
    });

    assert_eq!(v["success"], Value::Bool(false), "success should be false");
    let errors = v["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty(), "errors should be populated");
    let msg = errors[0]["message"].as_str().expect("message");
    assert!(
        msg.contains("initial") && msg.contains("exceeded maximum budget"),
        "expected initial-budget error message, got: {msg}",
    );
    // Output files were written before the budget check, so the array
    // should be populated even though the build is reported as failed.
    assert!(
        !v["output_files"]
            .as_array()
            .expect("output_files")
            .is_empty(),
        "outputs should still be reported on budget failure",
    );
}

#[test]
fn under_budget_production_build_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 10 MB budget — easily under for the trivial fixture above.
    write_fixture(
        dir.path(),
        r#"[{"type": "initial", "maximumError": "10mb"}]"#,
    );

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out_dir = dir.path().join("dist");
    let output = Command::new(bin)
        .args(["build", "--project"])
        .arg(dir.path().join("tsconfig.json"))
        .arg("--out-dir")
        .arg(&out_dir)
        .args(["-c", "production", "--output-json"])
        .output()
        .expect("spawn ngc-rs build");

    assert_eq!(output.status.code(), Some(0), "build should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let v: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["success"], Value::Bool(true));
    assert_eq!(v["errors"].as_array().expect("errors").len(), 0);
}

#[test]
fn warning_only_budget_does_not_fail_the_build() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 1-byte warning, no error threshold — should warn but exit 0.
    write_fixture(dir.path(), r#"[{"type": "initial", "maximumWarning": 1}]"#);

    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let out_dir = dir.path().join("dist");
    let output = Command::new(bin)
        .args(["build", "--project"])
        .arg(dir.path().join("tsconfig.json"))
        .arg("--out-dir")
        .arg(&out_dir)
        .args(["-c", "production", "--output-json"])
        .output()
        .expect("spawn ngc-rs build");

    assert_eq!(output.status.code(), Some(0), "warning should not fail");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let v: Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["success"], Value::Bool(true));
    let warnings = v["warnings"].as_array().expect("warnings array");
    assert!(!warnings.is_empty(), "warning should be present");
    assert_eq!(
        warnings[0]["severity"],
        Value::String("warning".to_string()),
    );
}
