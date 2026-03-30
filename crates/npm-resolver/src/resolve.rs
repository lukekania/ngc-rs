//! Bare specifier and relative import resolution for npm packages.
//!
//! Resolves bare module specifiers (e.g. `@angular/core`) to their ESM entry
//! files in `node_modules`, and relative imports within packages to absolute paths.

use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};

use crate::package_json::{parse_specifier, resolve_package_entry};

/// Resolve a bare module specifier to an absolute file path.
///
/// Parses the specifier into package name and subpath, locates the package
/// in `node_modules`, and resolves its entry point via `package.json`.
pub fn resolve_bare_specifier(specifier: &str, project_root: &Path) -> NgcResult<PathBuf> {
    let (pkg_name, subpath) = parse_specifier(specifier);
    let node_modules = project_root.join("node_modules");
    let pkg_dir = node_modules.join(&pkg_name);

    if !pkg_dir.is_dir() {
        return Err(NgcError::NpmResolutionError {
            specifier: specifier.to_string(),
            message: format!("package directory not found: {}", pkg_dir.display()),
        });
    }

    resolve_package_entry(&pkg_dir, &subpath)
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
    use std::fs;

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

        let result = resolve_bare_specifier("@angular/core", dir.path()).unwrap();
        assert!(result.ends_with("fesm2022/core.mjs"));
    }

    #[test]
    fn test_resolve_unscoped_package() {
        let dir = tempfile::tempdir().unwrap();
        setup_mock_packages(dir.path());

        let result = resolve_bare_specifier("rxjs", dir.path()).unwrap();
        assert!(result.ends_with("dist/esm5/index.js"));
    }

    #[test]
    fn test_resolve_subpath_import() {
        let dir = tempfile::tempdir().unwrap();
        setup_mock_packages(dir.path());

        let result = resolve_bare_specifier("rxjs/operators", dir.path()).unwrap();
        assert!(result.ends_with("dist/esm5/operators/index.js"));
    }

    #[test]
    fn test_resolve_missing_package() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_bare_specifier("nonexistent-pkg", dir.path());
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
}
