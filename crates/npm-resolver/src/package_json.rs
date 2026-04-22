//! Package.json parsing for npm module resolution.
//!
//! Reads a package's `package.json` and resolves the ESM entry point for a
//! given subpath using the `exports`, `module`, and `main` fields.
//!
//! Matches the Node.js Package Exports spec: the `exports` object is walked
//! in the order its keys appear in the JSON source, and the first key that
//! is either `"default"` or a member of the active condition set wins.
//! Values may themselves be condition objects (nested), a string, or a
//! `null` meaning the path is explicitly blocked.

use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};

/// Default conditions for a production browser bundle.
///
/// Order is immaterial — the exports object's key order is what Node uses
/// to decide precedence. What matters here is membership.
pub const PRODUCTION_BROWSER_CONDITIONS: &[&str] =
    &["browser", "module", "import", "production", "default"];

/// Default conditions for a development browser bundle.
pub const DEVELOPMENT_BROWSER_CONDITIONS: &[&str] =
    &["browser", "module", "import", "development", "default"];

/// Pick the default condition set for a build configuration name.
///
/// `"production"` activates the `production` condition; anything else (including
/// `"development"` or `None`) activates `development`. Both sets include
/// `browser`, `module`, `import`, and `default`.
pub fn conditions_for_configuration(configuration: Option<&str>) -> &'static [&'static str] {
    match configuration {
        Some("production") => PRODUCTION_BROWSER_CONDITIONS,
        _ => DEVELOPMENT_BROWSER_CONDITIONS,
    }
}

/// Resolve the ESM entry point for a package given its directory and a subpath.
///
/// Follows the Node.js module resolution algorithm:
/// 1. Check `exports` field for the subpath under the active conditions
/// 2. Fall back to `module` field (ESM entry)
/// 3. Fall back to `main` field
/// 4. Fall back to `index.js` / `index.mjs`
///
/// The `subpath` should be `"."` for the package root, or `"./sub"` for subpath imports.
pub fn resolve_package_entry(
    pkg_dir: &Path,
    subpath: &str,
    conditions: &[&str],
) -> NgcResult<PathBuf> {
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
        if let Some(entry) = resolve_exports(exports, subpath, conditions) {
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
/// - Sugar-form condition object at root: `"exports": { "import": "./m.mjs", "default": "./m.js" }`
/// - Subpath map: `"exports": { ".": ..., "./sub": ... }`
/// - Nested conditions: `{ "browser": { "import": "./b.mjs" } }`
/// - Pattern trailers: `"./feature/*": { "default": "./feat/*.js" }` — the
///   longest matching prefix wins, per the Node spec.
fn resolve_exports(
    exports: &serde_json::Value,
    subpath: &str,
    conditions: &[&str],
) -> Option<String> {
    match exports {
        // "exports": "./dist/index.js" — only matches root subpath
        serde_json::Value::String(s) if subpath == "." => Some(s.clone()),

        // "exports": [...] — array sugar; first resolvable wins
        serde_json::Value::Array(arr) if subpath == "." => {
            arr.iter().find_map(|v| resolve_target(v, None, conditions))
        }

        // "exports": { ".": ..., "./sub": ... } OR condition-keyed sugar
        serde_json::Value::Object(map) => {
            let has_subpath_keys = map.keys().any(|k| k.starts_with('.'));

            if has_subpath_keys {
                // Direct lookup first — pattern with no trailing "*"
                if let Some(entry) = map.get(subpath) {
                    return resolve_target(entry, None, conditions);
                }

                // Pattern trailers: longest-prefix match wins, per Node spec.
                let mut best: Option<(&str, &serde_json::Value)> = None;
                for (pattern, value) in map {
                    let Some(prefix) = pattern.strip_suffix('*') else {
                        continue;
                    };
                    if !subpath.starts_with(prefix) {
                        continue;
                    }
                    match best {
                        Some((cur, _)) if cur.len() >= prefix.len() => {}
                        _ => best = Some((prefix, value)),
                    }
                }
                if let Some((prefix, value)) = best {
                    let rest = &subpath[prefix.len()..];
                    return resolve_target(value, Some(rest), conditions);
                }

                None
            } else if subpath == "." {
                // Sugar: root entry whose keys are conditions.
                resolve_target(exports, None, conditions)
            } else {
                None
            }
        }

        _ => None,
    }
}

/// Resolve a conditional export value to a file path string, honouring the
/// active condition set.
///
/// The rules mirror Node's [PACKAGE_EXPORTS_RESOLVE] /
/// [PACKAGE_TARGET_RESOLVE] algorithm:
/// - `String` → the target path (with `*` substituted if we're in a pattern match).
/// - `Object` → iterate the keys in insertion order; the first key that is
///   either `"default"` or a member of `conditions` whose value resolves to
///   a non-null target wins. Non-string/non-object values are skipped.
/// - `Array` → alternate targets; first resolvable wins.
/// - `Null` → explicitly blocked; return None.
fn resolve_target(
    value: &serde_json::Value,
    pattern_rest: Option<&str>,
    conditions: &[&str],
) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            // Guard against accidentally picking up type declarations as a
            // runtime entry. If the package only offers a .d.ts and no
            // runtime alternative, this returns None and the caller falls
            // back to `module` / `main`.
            if s.ends_with(".d.ts") || s.ends_with(".d.mts") || s.ends_with(".d.cts") {
                return None;
            }
            Some(match pattern_rest {
                Some(rest) => s.replace('*', rest),
                None => s.clone(),
            })
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                // `types` never points at a runtime file; skip unless the
                // caller explicitly asked for it.
                if key == "types" && !conditions.contains(&"types") {
                    continue;
                }
                if key == "default" || conditions.contains(&key.as_str()) {
                    if let Some(resolved) = resolve_target(val, pattern_rest, conditions) {
                        return Some(resolved);
                    }
                }
            }
            None
        }
        serde_json::Value::Array(arr) => arr
            .iter()
            .find_map(|v| resolve_target(v, pattern_rest, conditions)),
        serde_json::Value::Null => None,
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

    const DEV: &[&str] = DEVELOPMENT_BROWSER_CONDITIONS;
    const PROD: &[&str] = PRODUCTION_BROWSER_CONDITIONS;

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
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, "./operators", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, "./locales/de", DEV).unwrap();
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
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(result.ends_with("esm/index.mjs"));
    }

    // --- New coverage: browser / node / development / production ---

    #[test]
    fn test_browser_condition_picks_browser_over_default() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "browser-pkg",
            r#"{ "exports": { ".": { "node": "./node/index.mjs", "browser": "./browser/index.mjs", "default": "./dist/index.mjs" } } }"#,
            &[
                ("node/index.mjs", "export const x = 'node';"),
                ("browser/index.mjs", "export const x = 'browser';"),
                ("dist/index.mjs", "export const x = 'default';"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/browser-pkg");
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(
            result.ends_with("browser/index.mjs"),
            "expected browser branch, got {}",
            result.display()
        );
    }

    #[test]
    fn test_node_condition_picks_node_when_browser_inactive() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "node-pkg",
            r#"{ "exports": { ".": { "node": "./node/index.mjs", "default": "./dist/index.mjs" } } }"#,
            &[
                ("node/index.mjs", "export const x = 'node';"),
                ("dist/index.mjs", "export const x = 'default';"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/node-pkg");
        let result = resolve_package_entry(&pkg_dir, ".", &["node", "import", "default"]).unwrap();
        assert!(result.ends_with("node/index.mjs"));
    }

    #[test]
    fn test_production_vs_development_pick_different_files() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "envmode-pkg",
            r#"{ "exports": { ".": { "development": "./dev/index.mjs", "production": "./prod/index.mjs", "default": "./dist/index.mjs" } } }"#,
            &[
                ("dev/index.mjs", "export const x = 'dev';"),
                ("prod/index.mjs", "export const x = 'prod';"),
                ("dist/index.mjs", "export const x = 'default';"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/envmode-pkg");
        let dev_res = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        let prod_res = resolve_package_entry(&pkg_dir, ".", PROD).unwrap();
        assert!(dev_res.ends_with("dev/index.mjs"), "{}", dev_res.display());
        assert!(
            prod_res.ends_with("prod/index.mjs"),
            "{}",
            prod_res.display()
        );
    }

    #[test]
    fn test_deeply_nested_browser_production_import() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "deep-pkg",
            r#"{
              "exports": {
                ".": {
                  "browser": {
                    "production": {
                      "import": "./b-prod/index.mjs",
                      "default": "./b-prod/fallback.js"
                    },
                    "development": {
                      "import": "./b-dev/index.mjs"
                    },
                    "default": "./b/index.mjs"
                  },
                  "default": "./dist/index.mjs"
                }
              }
            }"#,
            &[
                ("b-prod/index.mjs", "export const x = 'bp';"),
                ("b-dev/index.mjs", "export const x = 'bd';"),
                ("b/index.mjs", "export const x = 'b';"),
                ("dist/index.mjs", "export const x = 'd';"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/deep-pkg");
        let prod_res = resolve_package_entry(&pkg_dir, ".", PROD).unwrap();
        let dev_res = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(prod_res.ends_with("b-prod/index.mjs"));
        assert!(dev_res.ends_with("b-dev/index.mjs"));
    }

    #[test]
    fn test_first_match_wins_order_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        // `module` comes before `default` in the source — module must win
        // because both are active in the condition set.
        create_mock_package(
            dir.path(),
            "order-pkg",
            r#"{ "exports": { ".": { "module": "./esm/index.mjs", "default": "./cjs/index.js" } } }"#,
            &[
                ("esm/index.mjs", "export const x = 1;"),
                ("cjs/index.js", "module.exports = {};"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/order-pkg");
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(result.ends_with("esm/index.mjs"));
    }

    #[test]
    fn test_default_considered_last_even_if_listed_first() {
        let dir = tempfile::tempdir().unwrap();
        // "default" is in the JSON before "browser". Node spec says keys
        // are still walked top-down; the first match wins. So `default`
        // should win here because it matches first in source order.
        create_mock_package(
            dir.path(),
            "default-first",
            r#"{ "exports": { ".": { "default": "./dist/index.mjs", "browser": "./browser/index.mjs" } } }"#,
            &[
                ("dist/index.mjs", "export const x = 'd';"),
                ("browser/index.mjs", "export const x = 'b';"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/default-first");
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(result.ends_with("dist/index.mjs"));
    }

    #[test]
    fn test_null_target_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "blocked-pkg",
            r#"{ "exports": { ".": { "browser": null, "default": "./dist/index.mjs" } } }"#,
            &[("dist/index.mjs", "export const x = 1;")],
        );

        let pkg_dir = dir.path().join("node_modules/blocked-pkg");
        let result = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(result.ends_with("dist/index.mjs"));
    }

    #[test]
    fn test_pattern_trailer_under_nested_conditions() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "pat-pkg",
            r#"{ "exports": { "./feature/*": { "browser": { "import": "./browser/feat/*.mjs" }, "default": "./feat/*.js" } } }"#,
            &[
                ("browser/feat/foo.mjs", "export const x = 1;"),
                ("feat/foo.js", "module.exports = {};"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/pat-pkg");
        let result = resolve_package_entry(&pkg_dir, "./feature/foo", DEV).unwrap();
        assert!(
            result.ends_with("browser/feat/foo.mjs"),
            "{}",
            result.display()
        );
    }

    #[test]
    fn test_angular_package_shape() {
        // Mirrors how @angular/core lays out its exports today.
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "@angular/core",
            r#"{
              "exports": {
                "./package.json": { "default": "./package.json" },
                ".": {
                  "types": "./index.d.ts",
                  "default": "./fesm2022/core.mjs"
                },
                "./testing": {
                  "types": "./testing/index.d.ts",
                  "default": "./fesm2022/testing.mjs"
                }
              }
            }"#,
            &[
                ("fesm2022/core.mjs", "export const Component = {};"),
                ("fesm2022/testing.mjs", "export const TestBed = {};"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/@angular/core");
        let root = resolve_package_entry(&pkg_dir, ".", PROD).unwrap();
        let testing = resolve_package_entry(&pkg_dir, "./testing", PROD).unwrap();
        assert!(root.ends_with("fesm2022/core.mjs"));
        assert!(testing.ends_with("fesm2022/testing.mjs"));
    }

    #[test]
    fn test_react_like_dev_prod_split() {
        // Mirrors react's exports shape: separate development and production
        // files keyed by the `production` / `development` conditions.
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "reactish",
            r#"{
              "exports": {
                ".": {
                  "browser": {
                    "development": "./dev/reactish.development.mjs",
                    "production": "./prod/reactish.production.mjs",
                    "default": "./prod/reactish.production.mjs"
                  },
                  "default": "./index.js"
                }
              }
            }"#,
            &[
                ("dev/reactish.development.mjs", "export const v = 'dev';"),
                ("prod/reactish.production.mjs", "export const v = 'prod';"),
                ("index.js", "module.exports = {};"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/reactish");
        let prod = resolve_package_entry(&pkg_dir, ".", PROD).unwrap();
        let dev = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(prod.ends_with("reactish.production.mjs"));
        assert!(dev.ends_with("reactish.development.mjs"));
    }

    #[test]
    fn test_array_alternate_targets() {
        let dir = tempfile::tempdir().unwrap();
        create_mock_package(
            dir.path(),
            "alt-pkg",
            r#"{ "exports": { ".": [ { "browser": "./a.mjs" }, "./b.mjs" ] } }"#,
            &[
                ("a.mjs", "export const x = 'a';"),
                ("b.mjs", "export const x = 'b';"),
            ],
        );

        let pkg_dir = dir.path().join("node_modules/alt-pkg");
        let browser = resolve_package_entry(&pkg_dir, ".", DEV).unwrap();
        assert!(browser.ends_with("a.mjs"));
        // Without the browser condition the first alt yields None, so the
        // walker falls back to the string "./b.mjs".
        let no_browser =
            resolve_package_entry(&pkg_dir, ".", &["module", "import", "default"]).unwrap();
        assert!(no_browser.ends_with("b.mjs"));
    }
}
