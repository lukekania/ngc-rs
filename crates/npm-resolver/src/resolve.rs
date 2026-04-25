//! Bare specifier and relative import resolution for npm packages.
//!
//! Resolves bare module specifiers (e.g. `@angular/core`) to their ESM entry
//! files in `node_modules`, and relative imports within packages to absolute paths.

use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};

use crate::package_json::{match_imports_field, parse_specifier, resolve_package_entry};

/// Resolve a bare module specifier to an absolute file path.
///
/// Parses the specifier into package name and subpath, locates the package
/// in `node_modules`, and resolves its entry point via `package.json`.
/// `conditions` is the active condition set used by the exports field
/// matcher — see [`crate::package_json`] for the built-in sets.
pub fn resolve_bare_specifier(
    specifier: &str,
    project_root: &Path,
    conditions: &[&str],
) -> NgcResult<PathBuf> {
    let (pkg_name, subpath) = parse_specifier(specifier);
    let node_modules = project_root.join("node_modules");
    let pkg_dir = node_modules.join(&pkg_name);

    if !pkg_dir.is_dir() {
        return Err(NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("package directory not found: {}", pkg_dir.display()),
        });
    }

    resolve_package_entry(&pkg_dir, &subpath, conditions)
}

/// Resolve a `#`-prefixed subpath import against the nearest ancestor
/// `package.json` that owns the importing file.
///
/// Node's resolver scopes `imports` to the importing package, not the package
/// being imported *from*. So we walk up from `from_file` (or from
/// `project_root` when a top-level specifier has no anchoring file) until we
/// hit a `package.json`, then consult its `imports` field.
///
/// Targets that start with `./` / `../` / `/` are resolved relative to the
/// owning package directory. Anything else is treated as a bare specifier and
/// routed through the regular node_modules resolver.
pub fn resolve_subpath_import(
    specifier: &str,
    from_file: Option<&Path>,
    project_root: &Path,
    conditions: &[&str],
) -> NgcResult<PathBuf> {
    let start: &Path = from_file.and_then(|f| f.parent()).unwrap_or(project_root);
    let pkg_dir = find_ancestor_pkg_dir(start).ok_or_else(|| NgcError::NpmResolutionError {
        specifier: specifier.to_string(),
        message: format!("no ancestor package.json from {}", start.display()),
    })?;

    let pkg_json_path = pkg_dir.join("package.json");
    let content = std::fs::read_to_string(&pkg_json_path).map_err(|e| NgcError::Io {
        path: pkg_json_path.clone(),
        source: e,
    })?;
    let pkg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("invalid package.json at {}: {e}", pkg_json_path.display()),
        })?;

    let imports = pkg
        .get("imports")
        .ok_or_else(|| NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("no \"imports\" field in {}", pkg_json_path.display()),
        })?;

    let target = match_imports_field(imports, specifier, conditions).ok_or_else(|| {
        NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("no matching entry in {}", pkg_json_path.display()),
        }
    })?;

    if target.starts_with("./") || target.starts_with("../") || target.starts_with('/') {
        let rel = target.trim_start_matches('/');
        let resolved = pkg_dir.join(rel);
        if resolved.is_file() {
            return Ok(resolved);
        }
        if let Some(with_ext) = crate::package_json::try_extensions(&resolved) {
            return Ok(with_ext);
        }
        return Err(NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("imports target does not exist: {}", resolved.display()),
        });
    }

    resolve_bare_specifier(&target, project_root, conditions)
}

/// Walk upwards from `start` looking for a directory that contains
/// `package.json`. Returns the directory path if found.
fn find_ancestor_pkg_dir(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(d) = cur {
        if d.join("package.json").is_file() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Resolve a relative import specifier from within an npm package file.
///
/// Given a specifier like `'./_router-chunk.mjs'` and the importing file's path,
/// resolves to the absolute path of the target file.
pub fn resolve_relative_import(specifier: &str, from_file: &Path) -> NgcResult<PathBuf> {
    let from_dir = from_file
        .parent()
        .ok_or_else(|| NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("cannot determine directory of {}", from_file.display()),
        })?;

    let base = from_dir.join(specifier);

    // Try exact path first
    if base.is_file() {
        return Ok(base);
    }

    // Try with extensions
    for ext in &["mjs", "js", "cjs"] {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // Try as directory with index
    for index in &["index.mjs", "index.js"] {
        let candidate = base.join(index);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // Try appending extensions to paths that already have an extension
    // (e.g., specifier is "./foo.bar" but file is "foo.bar.js")
    let base_str = base.to_string_lossy();
    for ext in &[".mjs", ".js"] {
        let candidate = PathBuf::from(format!("{base_str}{ext}"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(NgcError::NpmResolutionError {
        specifier: specifier.to_string(),
        message: format!(
            "could not resolve relative import from {}",
            from_file.display()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_json::DEVELOPMENT_BROWSER_CONDITIONS;
    use std::fs;

    const DEV: &[&str] = DEVELOPMENT_BROWSER_CONDITIONS;

    fn setup_mock_packages(dir: &Path) {
        // @angular/core
        let core_dir = dir.join("node_modules/@angular/core");
        fs::create_dir_all(core_dir.join("fesm2022")).unwrap();
        fs::write(
            core_dir.join("package.json"),
            r#"{ "exports": { ".": { "default": "./fesm2022/core.mjs" } } }"#,
        )
        .unwrap();
        fs::write(
            core_dir.join("fesm2022/core.mjs"),
            "export const Component = {};\n",
        )
        .unwrap();

        // rxjs with subpath
        let rxjs_dir = dir.join("node_modules/rxjs");
        fs::create_dir_all(rxjs_dir.join("dist/esm5/operators")).unwrap();
        fs::write(
            rxjs_dir.join("package.json"),
            r#"{ "exports": { ".": { "default": "./dist/esm5/index.js" }, "./operators": { "default": "./dist/esm5/operators/index.js" } } }"#,
        )
        .unwrap();
        fs::write(
            rxjs_dir.join("dist/esm5/index.js"),
            "export const of = () => {};\n",
        )
        .unwrap();
        fs::write(
            rxjs_dir.join("dist/esm5/operators/index.js"),
            "export const map = () => {};\n",
        )
        .unwrap();
    }

    #[test]
    fn test_resolve_scoped_package() {
        let dir = tempfile::tempdir().unwrap();
        setup_mock_packages(dir.path());

        let result = resolve_bare_specifier("@angular/core", dir.path(), DEV).unwrap();
        assert!(result.ends_with("fesm2022/core.mjs"));
    }

    #[test]
    fn test_resolve_unscoped_package() {
        let dir = tempfile::tempdir().unwrap();
        setup_mock_packages(dir.path());

        let result = resolve_bare_specifier("rxjs", dir.path(), DEV).unwrap();
        assert!(result.ends_with("dist/esm5/index.js"));
    }

    #[test]
    fn test_resolve_subpath_import() {
        let dir = tempfile::tempdir().unwrap();
        setup_mock_packages(dir.path());

        let result = resolve_bare_specifier("rxjs/operators", dir.path(), DEV).unwrap();
        assert!(result.ends_with("dist/esm5/operators/index.js"));
    }

    #[test]
    fn test_resolve_missing_package() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_bare_specifier("nonexistent-pkg", dir.path(), DEV);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_relative_import_exact() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("pkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("chunk.mjs"), "export const x = 1;").unwrap();
        fs::write(pkg_dir.join("main.mjs"), "import './chunk.mjs';").unwrap();

        let from = pkg_dir.join("main.mjs");
        let result = resolve_relative_import("./chunk.mjs", &from).unwrap();
        assert!(result.ends_with("chunk.mjs"));
    }

    #[test]
    fn test_resolve_relative_import_with_extension_inference() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("pkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("utils.js"), "export const y = 2;").unwrap();
        fs::write(pkg_dir.join("main.mjs"), "import './utils';").unwrap();

        let from = pkg_dir.join("main.mjs");
        let result = resolve_relative_import("./utils", &from).unwrap();
        assert!(result.ends_with("utils.js"));
    }

    #[test]
    fn test_resolve_relative_import_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("pkg");
        fs::create_dir_all(pkg_dir.join("sub")).unwrap();
        fs::write(pkg_dir.join("shared.mjs"), "export const z = 3;").unwrap();
        fs::write(pkg_dir.join("sub/child.mjs"), "import '../shared.mjs';").unwrap();

        let from = pkg_dir.join("sub/child.mjs");
        let result = resolve_relative_import("../shared.mjs", &from).unwrap();
        assert!(result.ends_with("shared.mjs"));
    }

    // --- subpath imports (`#`-prefixed) ---

    #[test]
    fn test_subpath_import_literal_from_project_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r##"{ "imports": { "#internal/helper": "./src/internal/helper.js" } }"##,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src/internal")).unwrap();
        fs::write(
            dir.path().join("src/internal/helper.js"),
            "export const helper = 1;",
        )
        .unwrap();

        let result = resolve_subpath_import("#internal/helper", None, dir.path(), DEV).unwrap();
        assert!(result.ends_with("src/internal/helper.js"));
    }

    #[test]
    fn test_subpath_import_wildcard_substitution() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r##"{ "imports": { "#internal/*": "./src/internal/*.js" } }"##,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src/internal")).unwrap();
        fs::write(
            dir.path().join("src/internal/foo.js"),
            "export const f = 1;",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/internal/bar.js"),
            "export const b = 2;",
        )
        .unwrap();

        let foo = resolve_subpath_import("#internal/foo", None, dir.path(), DEV).unwrap();
        let bar = resolve_subpath_import("#internal/bar", None, dir.path(), DEV).unwrap();
        assert!(foo.ends_with("src/internal/foo.js"));
        assert!(bar.ends_with("src/internal/bar.js"));
    }

    #[test]
    fn test_subpath_import_conditional_picks_browser() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r##"{
              "imports": {
                "#dep": {
                  "node": "./node/dep.js",
                  "browser": "./browser/dep.mjs",
                  "default": "./default/dep.js"
                }
              }
            }"##,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("browser")).unwrap();
        fs::write(dir.path().join("browser/dep.mjs"), "export const d = 'b';").unwrap();

        let result = resolve_subpath_import("#dep", None, dir.path(), DEV).unwrap();
        assert!(result.ends_with("browser/dep.mjs"));
    }

    #[test]
    fn test_subpath_import_scopes_to_nearest_ancestor() {
        // Nested package.json at node_modules/dep owns any `#` specifier used
        // inside dep/dist/file.js — not the project-root package.json.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r##"{ "imports": { "#alias": "./project/alias.js" } }"##,
        )
        .unwrap();

        let dep_dir = dir.path().join("node_modules/dep");
        fs::create_dir_all(dep_dir.join("dist")).unwrap();
        fs::write(
            dep_dir.join("package.json"),
            r##"{ "imports": { "#alias": "./lib/alias.js" } }"##,
        )
        .unwrap();
        fs::create_dir_all(dep_dir.join("lib")).unwrap();
        fs::write(dep_dir.join("lib/alias.js"), "export const a = 'dep';").unwrap();

        let from = dep_dir.join("dist/main.js");
        let result = resolve_subpath_import("#alias", Some(&from), dir.path(), DEV).unwrap();
        assert!(
            result.ends_with("node_modules/dep/lib/alias.js"),
            "{}",
            result.display()
        );
    }

    #[test]
    fn test_subpath_import_bare_specifier_target() {
        // A `#` alias can point at a bare specifier, which must round-trip
        // through the node_modules resolver.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r##"{ "imports": { "#clone": "clone-pkg" } }"##,
        )
        .unwrap();
        let clone_dir = dir.path().join("node_modules/clone-pkg");
        fs::create_dir_all(&clone_dir).unwrap();
        fs::write(
            clone_dir.join("package.json"),
            r#"{ "module": "./index.mjs" }"#,
        )
        .unwrap();
        fs::write(clone_dir.join("index.mjs"), "export default 1;").unwrap();

        let result = resolve_subpath_import("#clone", None, dir.path(), DEV).unwrap();
        assert!(result.ends_with("node_modules/clone-pkg/index.mjs"));
    }

    #[test]
    fn test_subpath_import_no_match_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r##"{ "imports": { "#a": "./a.js" } }"##,
        )
        .unwrap();
        let err = resolve_subpath_import("#missing", None, dir.path(), DEV);
        assert!(err.is_err());
    }
}
