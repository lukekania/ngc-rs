//! Angular linker for partially compiled npm packages.
//!
//! Angular npm packages ship in a "partially compiled" FESM format using
//! `ɵɵngDeclare*` calls. This crate transforms those declarations into their
//! fully AOT-compiled equivalents (`ɵɵdefine*` calls), which Angular's runtime
//! can execute without JIT compilation.
//!
//! ## Usage
//! ```ignore
//! use std::collections::HashMap;
//! use std::path::PathBuf;
//! use ngc_linker::ModuleRegistry;
//!
//! let mut modules: HashMap<PathBuf, String> = HashMap::new();
//! // ... populate with npm module sources ...
//! let registry = ModuleRegistry::new();
//! let stats = ngc_linker::link_npm_modules(&mut modules, &project_root, &registry)?;
//! println!("Linked {} files", stats.files_linked);
//! ```

mod class_metadata;
mod component;
mod directive;
mod factory;
/// Post-link pass that expands NgModule references in component `dependencies`
/// arrays using the [`module_registry::ModuleRegistry`].
pub mod flatten;
mod injectable;
mod injector;
mod metadata;
/// Build-time NgModule registry and import-flattening primitives.
pub mod module_registry;
mod ng_module;
mod pipe;
/// Map of publicly-exported npm identifier → best import specifier.
pub mod public_exports;
mod selector;
/// Low-level linking API for transforming a single source file.
pub mod transform;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ngc_diagnostics::NgcResult;
use rayon::prelude::*;

pub use module_registry::ModuleRegistry;
pub use public_exports::PublicExports;

/// Statistics from the linking process.
#[derive(Debug, Clone, Default)]
pub struct LinkerStats {
    /// Number of npm files scanned for declarations.
    pub files_scanned: usize,
    /// Number of files that contained `ɵɵngDeclare*` calls and were linked.
    pub files_linked: usize,
    /// Number of `ɵɵdefineNgModule` calls scanned in Pass A.
    pub modules_registered: usize,
    /// Number of files whose component `dependencies` arrays were rewritten
    /// to expand NgModule references into flat directive/pipe lists.
    pub components_flattened: usize,
}

/// Link all partially compiled Angular npm modules in the modules map.
///
/// Scans all modules whose paths contain `node_modules` for `ɵɵngDeclare*`
/// calls. Files that contain declarations are parsed, transformed, and their
/// source is replaced in the map. Any `ɵɵngDeclareNgModule` calls encountered
/// are also registered in `registry` so a later flatten pass can resolve
/// standalone-component `imports` arrays.
///
/// Files without `ɵɵngDeclare` are skipped with zero overhead (fast string check).
pub fn link_npm_modules(
    modules: &mut HashMap<PathBuf, String>,
    _project_root: &Path,
    registry: &ModuleRegistry,
) -> NgcResult<LinkerStats> {
    // Snapshot (path, source) for every npm file, then narrow to those whose
    // source contains ɵɵngDeclare. Two-step so we can report accurate
    // scan/link counts and so the parallel transform phase owns its inputs.
    let npm_files: Vec<(&PathBuf, &String)> = modules
        .iter()
        .filter(|(path, _)| is_npm_module(path))
        .collect();
    let files_scanned = npm_files.len();

    let work: Vec<(PathBuf, String)> = npm_files
        .into_iter()
        .filter(|(_, source)| needs_linking(source))
        .map(|(p, s)| (p.clone(), s.clone()))
        .collect();

    // Transform in parallel. `transform::link_source` only reads the source
    // and writes into `ModuleRegistry` (internally RwLock-protected), so
    // per-file work is independent.
    let results: Vec<(PathBuf, Option<String>)> = work
        .par_iter()
        .map(|(path, source)| -> NgcResult<(PathBuf, Option<String>)> {
            let linked = transform::link_source(source, path, registry)?;
            Ok((path.clone(), linked))
        })
        .collect::<NgcResult<Vec<_>>>()?;

    // Apply the rewrites back to the module map serially.
    let mut files_linked = 0;
    for (path, maybe_new) in results {
        if let Some(new_source) = maybe_new {
            modules.insert(path.clone(), new_source);
            files_linked += 1;
            tracing::debug!(path = %path.display(), "linked Angular declarations");
        }
    }

    Ok(LinkerStats {
        files_scanned,
        files_linked,
        ..LinkerStats::default()
    })
}

/// Full Angular module-linking orchestrator.
///
/// Runs three passes in order:
/// 1. [`link_npm_modules`] — rewrites npm `ɵɵngDeclare*` partial declarations
///    into fully-compiled `ɵɵdefine*` calls, and registers each NgModule in
///    `registry` as a side effect.
/// 2. [`module_registry::scan_define_ng_modules`] — registers any remaining
///    `ɵɵdefineNgModule` calls (project AOT output, pre-compiled npm bundles).
/// 3. [`flatten::flatten_component_dependencies`] — walks every
///    `ɵɵdefineComponent` and expands NgModule references in its
///    `dependencies` array to transitively-exported directives/pipes. This is
///    what removes the need for the runtime `ɵɵgetComponentDepsFactory`
///    helper and fixes reactive-forms `NG01050` errors inside dialogs.
pub fn link_modules(
    modules: &mut HashMap<PathBuf, String>,
    project_root: &Path,
) -> NgcResult<LinkerStats> {
    let registry = ModuleRegistry::new();
    let public_exports = PublicExports::new();
    let mut stats = link_npm_modules(modules, project_root, &registry)?;

    // Build the public-exports index by scanning every npm file's top-level
    // `export { … }` statements. `PublicExports` is RwLock-protected so the
    // scan loop is a clean `par_iter` candidate.
    modules
        .par_iter()
        .filter(|(path, _)| is_npm_module(path))
        .for_each(|(path, source)| {
            public_exports.scan_file(source, path);
        });

    stats.modules_registered = module_registry::scan_define_ng_modules(modules, &registry)?;
    stats.components_flattened =
        flatten::flatten_component_dependencies(modules, &registry, &public_exports)?;
    Ok(stats)
}

/// Check whether a path is inside node_modules.
fn is_npm_module(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

/// Public version of [`is_npm_module`] for consumers (the CLI's post-link
/// scan needs to filter project files vs. npm files).
pub fn is_npm_path(path: &Path) -> bool {
    is_npm_module(path)
}

/// Fast check whether a source file might contain Angular partial declarations.
///
/// Uses a simple substring check — much faster than parsing.
fn needs_linking(source: &str) -> bool {
    source.contains("\u{0275}\u{0275}ngDeclare")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_npm_module() {
        assert!(is_npm_module(Path::new(
            "/project/node_modules/@angular/core/fesm2022/core.mjs"
        )));
        assert!(!is_npm_module(Path::new(
            "/project/src/app/app.component.ts"
        )));
    }

    #[test]
    fn test_needs_linking() {
        assert!(needs_linking(
            "static \u{0275}fac = i0.\u{0275}\u{0275}ngDeclareFactory({});"
        ));
        assert!(!needs_linking("export class MyService {}"));
    }

    #[test]
    fn test_link_npm_modules_empty() {
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/project/src/app.ts"),
            "export class App {}".to_string(),
        );
        let registry = ModuleRegistry::new();
        let stats = link_npm_modules(&mut modules, Path::new("/project"), &registry).unwrap();
        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.files_linked, 0);
    }

    #[test]
    fn test_link_npm_modules_no_declarations() {
        let mut modules = HashMap::new();
        modules.insert(
            PathBuf::from("/project/node_modules/lodash/lodash.mjs"),
            "export function chunk() {}".to_string(),
        );
        let registry = ModuleRegistry::new();
        let stats = link_npm_modules(&mut modules, Path::new("/project"), &registry).unwrap();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_linked, 0);
    }

    #[test]
    fn test_link_injectable() {
        let mut modules = HashMap::new();
        let source = r#"import * as i0 from '@angular/core';
class MyService {}
MyService.\u{0275}fac = i0.\u{0275}\u{0275}ngDeclareFactory({ minVersion: "12.0.0", version: "17.0.0", ngImport: i0, type: MyService, deps: [], target: 2 });
MyService.\u{0275}prov = i0.\u{0275}\u{0275}ngDeclareInjectable({ minVersion: "12.0.0", version: "17.0.0", ngImport: i0, type: MyService, providedIn: 'root' });
export { MyService };"#;
        // Use actual unicode chars for the ɵ symbols
        let source = source
            .replace(r"\u{0275}\u{0275}", "\u{0275}\u{0275}")
            .replace(r"\u{0275}fac", "\u{0275}fac")
            .replace(r"\u{0275}prov", "\u{0275}prov");

        modules.insert(
            PathBuf::from("/project/node_modules/@test/pkg/index.mjs"),
            source,
        );

        let registry = ModuleRegistry::new();
        let stats = link_npm_modules(&mut modules, Path::new("/project"), &registry).unwrap();
        assert_eq!(stats.files_linked, 1);

        let linked = modules
            .get(Path::new("/project/node_modules/@test/pkg/index.mjs"))
            .unwrap();
        assert!(!linked.contains("\u{0275}\u{0275}ngDeclare"));
        assert!(linked.contains("_Factory"));
        assert!(linked.contains("\u{0275}\u{0275}defineInjectable"));
    }

    #[test]
    fn test_link_npm_modules_registers_ng_module_exports() {
        let mut modules = HashMap::new();
        let source = "import * as i0 from '@angular/core';\n\
class ReactiveFormsModule {}\n\
ReactiveFormsModule.\u{0275}mod = i0.\u{0275}\u{0275}ngDeclareNgModule({ minVersion: \"14.0.0\", version: \"17.0.0\", ngImport: i0, type: ReactiveFormsModule, exports: [FormGroupDirective, FormControlName] });\n\
export { ReactiveFormsModule };";
        modules.insert(
            PathBuf::from("/project/node_modules/@angular/forms/index.mjs"),
            source.to_string(),
        );

        let registry = ModuleRegistry::new();
        let stats = link_npm_modules(&mut modules, Path::new("/project"), &registry).unwrap();
        assert_eq!(stats.files_linked, 1);
        assert!(registry.is_module("ReactiveFormsModule"));
        assert_eq!(
            registry.flatten("ReactiveFormsModule"),
            vec!["FormGroupDirective", "FormControlName"]
        );
    }
}
