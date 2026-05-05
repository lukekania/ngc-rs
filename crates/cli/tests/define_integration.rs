//! End-to-end test that the `define` map in `angular.json` reaches the
//! emitted bundle: identifiers are replaced with their JS source-fragment
//! values verbatim, and per-configuration overrides win over base options.
//!
//! Mirrors `@angular/build:application`'s contract for `define`: the
//! recorded value is treated as raw JavaScript source (so quoted JSON
//! strings produce string literals, bare numbers produce numeric literals,
//! and so on).

use std::fs;
use std::path::Path;
use std::process::Command;

fn write_fixture(root: &Path, base_define: &str, prod_define: Option<&str>) {
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
    // The substitutions happen at the IdentifierReference level, so we
    // write each define as a top-level identifier read.
    fs::write(
        src.join("main.ts"),
        "declare const __APP_API_URL__: string;\n\
         declare const __BUILD_VERSION__: string;\n\
         declare const __FEATURE_X__: boolean;\n\
         declare const __BUILD_NUMBER__: number;\n\
         console.log(__APP_API_URL__);\n\
         console.log(__BUILD_VERSION__);\n\
         console.log(__FEATURE_X__);\n\
         console.log(__BUILD_NUMBER__);\n",
    )
    .expect("write main.ts");

    let prod_block = match prod_define {
        Some(d) => format!(
            r#",
          "configurations": {{
            "production": {{
              "define": {d}
            }}
          }}"#
        ),
        None => String::new(),
    };

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
            "browser": "src/main.ts",
            "tsConfig": "tsconfig.json",
            "define": {base}
          }}{prod_block}
        }}
      }}
    }}
  }}
}}
"#,
        base = base_define,
        prod_block = prod_block,
    );
    fs::write(root.join("angular.json"), angular_json).expect("write angular.json");
}

fn run_build(root: &Path, out_dir: &Path, configuration: Option<&str>) -> (i32, String, String) {
    let bin = env!("CARGO_BIN_EXE_ngc-rs");
    let mut cmd = Command::new(bin);
    cmd.args(["build", "--project"])
        .arg(root.join("tsconfig.json"))
        .arg("--out-dir")
        .arg(out_dir);
    if let Some(c) = configuration {
        cmd.args(["-c", c]);
    }
    let output = cmd.output().expect("spawn ngc-rs build");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn read_main_bundle(out_dir: &Path) -> String {
    // The bundler may content-hash filenames in production, so search for
    // any `main*.js` rather than hard-coding the name.
    let mut candidates: Vec<_> = fs::read_dir(out_dir)
        .expect("read out_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "js")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("main"))
        })
        .collect();
    candidates.sort();
    let path = candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no main*.js found in {}", out_dir.display()));
    fs::read_to_string(&path).expect("read bundle")
}

#[test]
fn define_string_value_is_substituted_into_bundle() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_fixture(
        dir.path(),
        r#"{
            "__APP_API_URL__": "\"https://api.example.com\"",
            "__BUILD_VERSION__": "\"1.0.0-ngref\"",
            "__FEATURE_X__": "true",
            "__BUILD_NUMBER__": "42"
        }"#,
        None,
    );

    let out_dir = dir.path().join("dist");
    let (code, stdout, stderr) = run_build(dir.path(), &out_dir, None);
    assert_eq!(
        code, 0,
        "build failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let bundle = read_main_bundle(&out_dir);

    // String literal: the raw value from angular.json is `"\"…\""`, so the
    // emitted bundle must contain the unescaped JS string literal.
    assert!(
        bundle.contains(r#""https://api.example.com""#),
        "string-literal define missing from bundle:\n{bundle}"
    );
    assert!(
        bundle.contains(r#""1.0.0-ngref""#),
        "string-literal define missing from bundle:\n{bundle}"
    );
    // Boolean literal — the user-defined identifier should be folded to `true`.
    assert!(
        !bundle.contains("__FEATURE_X__"),
        "user-defined identifier `__FEATURE_X__` leaked into bundle:\n{bundle}"
    );
    // Numeric literal — substituted in place.
    assert!(
        !bundle.contains("__BUILD_NUMBER__"),
        "user-defined identifier `__BUILD_NUMBER__` leaked into bundle:\n{bundle}"
    );
    // Sanity: none of the user-define identifiers should appear in the bundle.
    assert!(
        !bundle.contains("__APP_API_URL__"),
        "user-defined identifier leaked into bundle:\n{bundle}"
    );
    assert!(
        !bundle.contains("__BUILD_VERSION__"),
        "user-defined identifier leaked into bundle:\n{bundle}"
    );
}

#[test]
fn define_per_configuration_override_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_fixture(
        dir.path(),
        r#"{
            "__APP_API_URL__": "\"https://dev.example.com\"",
            "__BUILD_VERSION__": "\"dev\"",
            "__FEATURE_X__": "false",
            "__BUILD_NUMBER__": "0"
        }"#,
        Some(
            r#"{
                "__APP_API_URL__": "\"https://prod.example.com\""
            }"#,
        ),
    );

    let out_dir = dir.path().join("dist");
    let (code, stdout, stderr) = run_build(dir.path(), &out_dir, Some("production"));
    assert_eq!(
        code, 0,
        "build failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let bundle = read_main_bundle(&out_dir);

    // The configuration override wins for collisions. The minifier may
    // rewrite the string-literal delimiter (`"…"` → `` `…` ``), so assert
    // on the unquoted payload only.
    assert!(
        bundle.contains("https://prod.example.com"),
        "production override missing:\n{bundle}"
    );
    assert!(
        !bundle.contains("https://dev.example.com"),
        "base value should be overridden:\n{bundle}"
    );
    // Base-only entry survives — the substituted value is the JS literal
    // `"dev"`, which the minifier may emit as `'dev'`, `"dev"`, or `` `dev` ``.
    let has_dev_literal =
        bundle.contains(r#""dev""#) || bundle.contains("'dev'") || bundle.contains("`dev`");
    assert!(
        has_dev_literal,
        "base define `__BUILD_VERSION__` missing:\n{bundle}"
    );
    // And the user-defined identifier was substituted, not left bare.
    assert!(
        !bundle.contains("__BUILD_VERSION__"),
        "user-defined identifier leaked into bundle:\n{bundle}"
    );
}
