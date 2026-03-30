//! Package.json parsing for npm module resolution.
//!
//! Reads a package's `package.json` and resolves the ESM entry point for a
//! given subpath using the `exports`, `module`, and `main` fields.

use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};

/// Resolve the ESM entry point for a package given its directory and a subpath.
///
/// Follows the Node.js module resolution algorithm:
/// 1. Check `exports` field for the subpath with `"default"` condition
/// 2. Fall back to `module` field (ESM entry)
/// 3. Fall back to `main` field
/// 4. Fall back to `index.js`
///
/// The `subpath` should be `"."` for the package root, or `"./sub"` for subpath imports.
pub fn resolve_package_entry(pkg_dir: &Path, subpath: &str) -> NgcResult<PathBuf> {
    let pkg_json_path = pkg_dir.join("package.json");
    let content = std::fs::read_to_string(&pkg_json_path).map_err(|e| NgcError::Io {
        path: pkg_json_path.clone(),
        source: e,
    })?;

    let pkg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| NgcError::NpmResolutionError {
            specifier: pkg_dir.display().to_string(),
            message: format!("invalid package.json: {e}"),
        })?;

    // 1. Try exports field
    if let Some(exports) = pkg.get("exports") {
        if let Some(entry) = resolve_exports(exports, subpath) {
            let resolved = pkg_dir.join(&entry);
            if resolved.is_file() {
                return Ok(resolved);
            }
            // Try with extensions
            if let Some(with_ext) = try_extensions(&resolved) {
                return Ok(with_ext);
            }
        }
    }

    // 2. Only for root subpath: try module, then main
    if subpath == "." {
        if let Some(module) = pkg.get("module").and_then(|v| v.as_str()) {
            let resolved = pkg_dir.join(module);
            if resolved.is_file() {
                return Ok(resolved);
            }
        }

        if let Some(main) = pkg.get("main").and_then(|v| v.as_str()) {
            let resolved = pkg_dir.join(main);
            if resolved.is_file() {
                return Ok(resolved);
            }
            if let Some(with_ext) = try_extensions(&resolved) {
                return Ok(with_ext);
            }
        }

        // 3. Fallback to index.js / index.mjs
        for index in &["index.mjs", "index.js"] {
            let candidate = pkg_dir.join(index);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(NgcError::NpmResolutionError {
        specifier: format!("{}/{}", pkg_dir.display(), subpath),
        message: "could not resolve entry point".to_string(),
    })
}

/// Resolve a subpath within the `exports` field of package.json.
///
/// Supports:
/// - String exports: `"exports": "./dist/index.js"`
/// - Object exports with conditions: `"exports": { ".": { "default": "./dist/index.js" } }`
/// - Nested conditions: `"exports": { ".": { "import": { "default": "./dist/index.mjs" } } }`
/// - Glob patterns: `"./locales/*": { "default": "./locales/*.js" }`
fn resolve_exports(exports: &serde_json::Value, subpath: &str) -> Option<String> {
    match exports {
        // "exports": "./dist/index.js" — only matches root subpath
        serde_json::Value::String(s) if subpath == "." => Some(s.clone()),

        // "exports": { ".": ..., "./sub": ... }
        serde_json::Value::Object(map) => {
            // Check if keys look like subpaths (start with ".")
            let has_subpath_keys = map.keys().any(|k| k.starts_with('.'));

            if has_subpath_keys {
                // Direct lookup first
                if let Some(entry) = map.get(subpath) {
                    return resolve_condition(entry);
                }

                // Try glob pattern matching: "./locales/*" matches "./locales/de"
                for (pattern, value) in map {
                    if let Some(prefix) = pattern.strip_suffix('*') {
                        if let Some(rest) = subpath.strip_prefix(prefix) {
                            if let Some(resolved) = resolve_condition(value) {
                                // Replace * in the resolved path with the matched rest
                                return Some(resolved.replace('*', rest));
                            }
                        }
                    }
                }

                None
            } else {
                // Keys are conditions (e.g., "import", "default", "types")
                // This is the root entry
                if subpath == "." {
                    resolve_condition(exports)
                } else {
                    None
                }
            }
        }

        _ => None,
    }
}

/// Resolve a conditional export value to a file path string.
///
/// Handles:
/// - Direct string: `"./dist/index.js"`
/// - Condition object: `{ "default": "./dist/index.js", "types": "./dist/index.d.ts" }`
/// - Nested conditions: `{ "import": { "default": "./dist/index.mjs" } }`
fn resolve_condition(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) => {
            // Prefer "default" condition (ESM), then "import", then "require"
            for key in &["default", "import", "require"] {
                if let Some(val) = map.get(*key) {
                    if let Some(resolved) = resolve_condition(val) {
                        // Skip .d.ts files
                        if !resolved.ends_with(".d.ts") {
                            return Some(resolved);
                        }
                    }
                }
            }
            // Try first non-types entry
            for (key, val) in map {
                if key == "types" {
                    continue;
                }
                if let Some(resolved) = resolve_condition(val) {
                    if !resolved.ends_with(".d.ts") {
                        return Some(resolved);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Try common ESM/JS extensions for a path.
fn try_extensions(base: &Path) -> Option<PathBuf> {
    for ext in &["mjs", "js", "cjs"] {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Try as directory with index files
    for index in &["index.mjs", "index.js"] {
        let candidate = base.join(index);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse a bare import specifier into package name and subpath.
///
/// Examples:
/// - `@angular/core` → (`@angular/core`, `"."`)
/// - `@angular/core/testing` → (`@angular/core`, `"./testing"`)
/// - `rxjs` → (`rxjs`, `"."`)
/// - `rxjs/operators` → (`rxjs`, `"./operators"`)
pub fn parse_specifier(specifier: &str) -> (String, String) {
    if specifier.starts_with('@') {
        // Scoped package: @scope/name or @scope/name/subpath
        let parts: Vec<&str> = specifier.splitn(3, '/').collect();
        if parts.len() >= 3 {
            let pkg = format!("{}/{}", parts[0], parts[1]);
            let sub = format!("./{}", parts[2]);
            (pkg, sub)
        } else {
            (specifier.to_string(), ".".to_string())
        }
    } else {
        // Unscoped: name or name/subpath
        let parts: Vec<&str> = specifier.splitn(2, '/').collect();
        if parts.len() == 2 {
            let pkg = parts[0].to_string();
            let sub = format!("./{}", parts[1]);
            (pkg, sub)
        } else {
            (specifier.to_string(), ".".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_mock_package(dir: &Path, name: &str, pkg_json: &str, files: &[(&str, &str)]) {
        let pkg_dir = dir.join("node_modules").join(name);
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("package.json"), pkg_json).unwrap();
        for (path, content) in files {
            let file_path = pkg_dir.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(file_path, content).unwrap();
        }
    }

    #[test]
    fn test_parse_specifier_scoped() {
        let (pkg, sub) = parse_specifier("@angular/core");
        assert_eq!(pkg, "@angular/core");
        assert_eq!(sub, ".");
    }

    #[test]
    fn test_parse_specifier_scoped_subpath() {
        let (pkg, sub) = parse_specifier("@angular/core/testing");
        assert_eq!(pkg, "@angular/core");
        assert_eq!(sub, "./testing");
    }

    #[test]
    fn test_parse_specifier_unscoped() {
        let (pkg, sub) = parse_specifier("rxjs");
        assert_eq!(pkg, "rxjs");
        assert_eq!(sub, ".");
    }

    #[test]
    fn test_parse_specifier_unscoped_subpath() {
        let (pkg, sub) = parse_specifier("rxjs/operators");
        assert_eq!(pkg, "rxjs");
        assert_eq!(sub, "./operators");
    }

    #[test]
    fn test_resolve_exports_string() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "simple-pkg",
            r#"{ "exports": "./dist/index.js" }"#,
            &[("dist/index.js", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/simple-pkg");
        let result = resolve_package_entry(&pkg_dir, ".").unwrap();
        assert!(result.ends_with("dist/index.js"));
    }

    #[test]
    fn test_resolve_exports_object_with_conditions() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "cond-pkg",
            r#"{ "exports": { ".": { "types": "./types/index.d.ts", "default": "./fesm2022/pkg.mjs" } } }"#,
            &[("fesm2022/pkg.mjs", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/cond-pkg");
        let result = resolve_package_entry(&pkg_dir, ".").unwrap();
        assert!(result.ends_with("fesm2022/pkg.mjs"));
    }

    #[test]
    fn test_resolve_exports_subpath() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "sub-pkg",
            r#"{ "exports": { ".": { "default": "./dist/index.js" }, "./operators": { "default": "./dist/operators/index.js" } } }"#,
            &[
                ("dist/index.js", "export const x = 1;"),
                ("dist/operators/index.js", "export const y = 2;"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/sub-pkg");
        let result = resolve_package_entry(&pkg_dir, "./operators").unwrap();
        assert!(result.ends_with("dist/operators/index.js"));
    }

    #[test]
    fn test_resolve_module_field_fallback() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "module-pkg",
            r#"{ "module": "./esm/index.mjs" }"#,
            &[("esm/index.mjs", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/module-pkg");
        let result = resolve_package_entry(&pkg_dir, ".").unwrap();
        assert!(result.ends_with("esm/index.mjs"));
    }

    #[test]
    fn test_resolve_main_field_fallback() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "main-pkg",
            r#"{ "main": "./lib/index.js" }"#,
            &[("lib/index.js", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/main-pkg");
        let result = resolve_package_entry(&pkg_dir, ".").unwrap();
        assert!(result.ends_with("lib/index.js"));
    }

    #[test]
    fn test_resolve_index_fallback() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "bare-pkg",
            r#"{ "name": "bare-pkg" }"#,
            &[("index.js", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/bare-pkg");
        let result = resolve_package_entry(&pkg_dir, ".").unwrap();
        assert!(result.ends_with("index.js"));
    }

    #[test]
    fn test_resolve_exports_glob_pattern() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "glob-pkg",
            r#"{ "exports": { ".": { "default": "./dist/index.js" }, "./locales/*": { "default": "./locales/*.js" } } }"#,
            &[
                ("dist/index.js", "export const x = 1;"),
                ("locales/de.js", "export default {};"),
                ("locales/en.js", "export default {};"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/glob-pkg");
        let result = resolve_package_entry(&pkg_dir, "./locales/de").unwrap();
        assert!(result.ends_with("locales/de.js"));
    }

    #[test]
    fn test_resolve_nested_conditions() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "nested-pkg",
            r#"{ "exports": { ".": { "import": { "types": "./types.d.ts", "default": "./esm/index.mjs" }, "require": "./cjs/index.js" } } }"#,
            &[("esm/index.mjs", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/nested-pkg");
        let result = resolve_package_entry(&pkg_dir, ".").unwrap();
        assert!(result.ends_with("esm/index.mjs"));
    }
}
