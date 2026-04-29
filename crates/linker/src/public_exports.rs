//! Map of publicly-exported npm identifier → best import specifier.
//!
//! ## Why this exists
//!
//! The flatten pass (see `flatten.rs`) expands `imports: [SomeModule]` in a
//! project component to its transitive list of exported directives/pipes, and
//! needs to ensure those identifiers are actually importable in the project
//! file. The naive approach — "append to the existing import from the same
//! npm package" — breaks for two well-known cases:
//!
//! 1. An NgModule's `ɵmod.exports` list may reference a sibling NgModule whose
//!    members live in a *different* npm subpath. Example:
//!    `DialogModule` from `@angular/cdk/dialog` re-exports `PortalModule`,
//!    whose members `CdkPortal` / `CdkPortalOutlet` are published under
//!    `@angular/cdk/portal`, *not* `@angular/cdk/dialog`.
//!    In `dialog.mjs` they are exported only under aliases (`ɵɵCdkPortal`),
//!    so `import { CdkPortal } from '@angular/cdk/dialog'` binds to
//!    `undefined` — triggering `NG0919` at runtime.
//! 2. An NgModule's `ɵmod.exports` may reference an internal class that isn't
//!    publicly exported from any npm package (e.g. `_EmptyOutletComponent`
//!    from `@angular/router`). Those must not be added to any project file
//!    import at all.
//!
//! Both cases are handled by consulting this map: for each identifier we want
//! to add, find the *actual* npm package that publicly exports it, and add
//! the import against that package. Identifiers with no public export are
//! dropped from the flattened dependency list.

use std::path::Path;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use oxc_allocator::Allocator;
use oxc_ast::ast::{ExportSpecifier, ModuleExportName, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Public-exports registry. Populated during npm linking from every
/// `node_modules/…` file's top-level `export { … }` statements.
#[derive(Debug, Default)]
pub struct PublicExports {
    /// Exported local name → preferred npm specifier.
    ///
    /// Preferred = first seen with a *canonical* specifier (see
    /// [`derive_specifier`]). Internal chunk files like
    /// `_router_module-chunk.mjs` are ignored.
    name_to_specifier: DashMap<String, String>,
}

impl PublicExports {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this identifier publicly exported from any tracked npm file?
    pub fn has(&self, name: &str) -> bool {
        self.name_to_specifier.contains_key(name)
    }

    /// Npm specifier that publicly exports `name`, or `None` if unknown.
    pub fn specifier_for(&self, name: &str) -> Option<String> {
        self.name_to_specifier.get(name).map(|r| r.value().clone())
    }

    /// Scan a single npm file's source for its top-level `export { … }` and
    /// `export … from '…'` statements, and record each exported local name
    /// with a specifier derived from `path`.
    ///
    /// Returns the number of names newly recorded (ignoring duplicates).
    pub fn scan_file(&self, source: &str, path: &Path) -> usize {
        let Some(specifier) = derive_specifier(path) else {
            return 0;
        };
        // Cheap substring check before parsing.
        if !source.contains("export") {
            return 0;
        }
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, source, SourceType::mjs()).parse();
        if !parsed.errors.is_empty() {
            return 0;
        }
        let mut exported = Vec::new();
        for stmt in &parsed.program.body {
            if let Statement::ExportNamedDeclaration(export) = stmt {
                for spec in &export.specifiers {
                    if let Some(name) = exported_name(spec) {
                        exported.push(name);
                    }
                }
            }
        }
        let mut added = 0;
        for name in exported {
            if let Entry::Vacant(v) = self.name_to_specifier.entry(name) {
                v.insert(specifier.clone());
                added += 1;
            }
        }
        added
    }

    /// Number of registered identifiers.
    pub fn len(&self) -> usize {
        self.name_to_specifier.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.name_to_specifier.is_empty()
    }
}

/// Return the exported *local* name of an export specifier.
///
/// For `export { Inner as Outer } from './x'` we want `Outer` (what downstream
/// consumers import). For `export { Foo }` we want `Foo`. Returns `None` for
/// non-identifier exports (string exports).
fn exported_name(spec: &ExportSpecifier<'_>) -> Option<String> {
    match &spec.exported {
        ModuleExportName::IdentifierName(id) => Some(id.name.to_string()),
        ModuleExportName::IdentifierReference(id) => Some(id.name.to_string()),
        ModuleExportName::StringLiteral(_) => None,
    }
}

/// Derive the best-guess npm import specifier from an npm file path.
///
/// Conventions handled:
/// - `node_modules/@scope/pkg/fesm2022/pkg.mjs` → `@scope/pkg`
/// - `node_modules/@scope/pkg/fesm2022/subpath.mjs` → `@scope/pkg/subpath`
///   (e.g. `@angular/cdk/portal`)
/// - `node_modules/pkg/fesm2022/pkg.mjs` → `pkg`
/// - `node_modules/@scope/pkg/fesm2022/pkg-scope-pkg.mjs` (e.g.
///   `@ngx-translate/core/fesm2022/ngx-translate-core.mjs`) → `@scope/pkg`
///   when the filename stem equals the dashed form of the full package name.
///
/// Skipped (returns `None`):
/// - Files whose stem starts with `_` (internal chunks, not publicly
///   consumable via any import path).
/// - Anything outside `node_modules/`.
pub fn derive_specifier(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let idx = s.find("/node_modules/")?;
    let rest = &s[idx + "/node_modules/".len()..];
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.is_empty() {
        return None;
    }
    let (pkg, after) = if parts[0].starts_with('@') {
        if parts.len() < 2 {
            return None;
        }
        (format!("{}/{}", parts[0], parts[1]), &parts[2..])
    } else {
        (parts[0].to_string(), &parts[1..])
    };

    // Drop fesm2022 / fesm2020 / esm2022 wrapper directories — they're layout
    // folders inside the package, not part of the public specifier.
    let after: Vec<&str> = after
        .iter()
        .copied()
        .filter(|p| !matches!(*p, "fesm2022" | "fesm2020" | "esm2022" | "esm2020"))
        .collect();

    if after.is_empty() {
        return Some(pkg);
    }
    let file = *after.last()?;
    let stem = file.strip_suffix(".mjs").unwrap_or(file);
    if stem.starts_with('_') || stem.contains("-chunk") {
        return None;
    }

    // Does the stem correspond to the package as a whole?
    let pkg_simple = pkg.split('/').next_back().unwrap_or(&pkg);
    // Dashed form: `@scope/pkg` → `scope-pkg`, `pkg` → `pkg`.
    let pkg_dashed = pkg.trim_start_matches('@').replace('/', "-");
    if stem == pkg_simple || stem == pkg_dashed {
        return Some(pkg);
    }
    // Otherwise treat as subpath: `@angular/cdk/portal` etc.
    Some(format!("{}/{}", pkg, stem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn derive_specifier_package_main() {
        assert_eq!(
            derive_specifier(&PathBuf::from(
                "/project/node_modules/@angular/forms/fesm2022/forms.mjs"
            )),
            Some("@angular/forms".to_string())
        );
    }

    #[test]
    fn derive_specifier_subpath() {
        assert_eq!(
            derive_specifier(&PathBuf::from(
                "/project/node_modules/@angular/cdk/fesm2022/portal.mjs"
            )),
            Some("@angular/cdk/portal".to_string())
        );
        assert_eq!(
            derive_specifier(&PathBuf::from(
                "/project/node_modules/@angular/cdk/fesm2022/dialog.mjs"
            )),
            Some("@angular/cdk/dialog".to_string())
        );
    }

    #[test]
    fn derive_specifier_dashed_main_form() {
        assert_eq!(
            derive_specifier(&PathBuf::from(
                "/project/node_modules/@ngx-translate/core/fesm2022/ngx-translate-core.mjs"
            )),
            Some("@ngx-translate/core".to_string())
        );
    }

    #[test]
    fn derive_specifier_skips_internal_chunks() {
        assert!(derive_specifier(&PathBuf::from(
            "/project/node_modules/@angular/cdk/fesm2022/_overlay-module-chunk.mjs"
        ))
        .is_none());
        assert!(derive_specifier(&PathBuf::from(
            "/project/node_modules/@angular/router/fesm2022/_router_module-chunk.mjs"
        ))
        .is_none());
    }

    #[test]
    fn derive_specifier_ignores_outside_node_modules() {
        assert!(derive_specifier(&PathBuf::from("/project/src/app/foo.ts")).is_none());
    }

    #[test]
    fn derive_specifier_handles_unscoped_pkg() {
        assert_eq!(
            derive_specifier(&PathBuf::from(
                "/project/node_modules/lodash/fesm2022/lodash.mjs"
            )),
            Some("lodash".to_string())
        );
    }

    #[test]
    fn scan_file_records_plain_export() {
        let reg = PublicExports::new();
        let src = "class A {}\nclass B {}\nexport { A, B };";
        let added = reg.scan_file(
            src,
            Path::new("/proj/node_modules/@angular/forms/fesm2022/forms.mjs"),
        );
        assert_eq!(added, 2);
        assert_eq!(reg.specifier_for("A"), Some("@angular/forms".into()));
        assert_eq!(reg.specifier_for("B"), Some("@angular/forms".into()));
    }

    #[test]
    fn scan_file_uses_exported_alias_not_local() {
        let reg = PublicExports::new();
        // Mirrors @angular/cdk/dialog: exports CdkPortal under the name ɵɵCdkPortal.
        // The public name is what downstream code imports, so we record
        // `ɵɵCdkPortal`, NOT `CdkPortal`.
        let src = "class CdkPortal {}\nexport { CdkPortal as \u{0275}\u{0275}CdkPortal };";
        reg.scan_file(
            src,
            Path::new("/proj/node_modules/@angular/cdk/fesm2022/dialog.mjs"),
        );
        assert_eq!(
            reg.specifier_for("\u{0275}\u{0275}CdkPortal"),
            Some("@angular/cdk/dialog".into())
        );
        // Plain `CdkPortal` is not publicly exported from dialog.
        assert!(reg.specifier_for("CdkPortal").is_none());
    }

    #[test]
    fn scan_file_records_reexport_from_subpath() {
        let reg = PublicExports::new();
        // Real portal.mjs declares CdkPortal and exports it by name.
        let src = "class CdkPortal {}\nexport { CdkPortal };";
        reg.scan_file(
            src,
            Path::new("/proj/node_modules/@angular/cdk/fesm2022/portal.mjs"),
        );
        assert_eq!(
            reg.specifier_for("CdkPortal"),
            Some("@angular/cdk/portal".into())
        );
    }

    #[test]
    fn scan_file_first_seen_wins() {
        let reg = PublicExports::new();
        reg.scan_file(
            "class X {}\nexport { X };",
            Path::new("/proj/node_modules/@a/first/fesm2022/first.mjs"),
        );
        reg.scan_file(
            "class X {}\nexport { X };",
            Path::new("/proj/node_modules/@a/second/fesm2022/second.mjs"),
        );
        assert_eq!(reg.specifier_for("X"), Some("@a/first".into()));
    }

    #[test]
    fn scan_file_skips_internal_chunk_file() {
        let reg = PublicExports::new();
        reg.scan_file(
            "class Internal {}\nexport { Internal };",
            Path::new("/proj/node_modules/@angular/router/fesm2022/_router_module-chunk.mjs"),
        );
        assert!(reg.specifier_for("Internal").is_none());
    }

    #[test]
    fn scan_file_ignores_string_exports() {
        // Weird but legal: `export { "default" } from './x'` — we skip.
        let reg = PublicExports::new();
        let src = "export { foo } from './other';";
        reg.scan_file(src, Path::new("/proj/node_modules/pkg/fesm2022/pkg.mjs"));
        assert_eq!(reg.specifier_for("foo"), Some("pkg".into()));
    }
}
