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
//!
//! let mut modules: HashMap<PathBuf, String> = HashMap::new();
//! // ... populate with npm module sources ...
//! let stats = ngc_linker::link_npm_modules(&mut modules, &project_root)?;
//! println!("Linked {} files", stats.files_linked);
//! ```

mod class_metadata;
mod component;
mod directive;
mod factory;
mod injectable;
mod injector;
mod metadata;
/// Build-time NgModule registry and import-flattening primitives.
pub mod module_registry;
mod ng_module;
mod pipe;
mod selector;
/// Low-level linking API for transforming a single source file.
pub mod transform;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ngc_diagnostics::NgcResult;

/// Statistics from the linking process.
#[derive(Debug, Clone)]
pub struct LinkerStats {
    /// Number of npm files scanned for declarations.
    pub files_scanned: usize,
    /// Number of files that contained `ɵɵngDeclare*` calls and were linked.
    pub files_linked: usize,
}

/// Link all partially compiled Angular npm modules in the modules map.
///
/// Scans all modules whose paths contain `node_modules` for `ɵɵngDeclare*`
/// calls. Files that contain declarations are parsed, transformed, and their
/// source is replaced in the map.
///
/// Files without `ɵɵngDeclare` are skipped with zero overhead (fast string check).
pub fn link_npm_modules(
    modules: &mut HashMap<PathBuf, String>,
    _project_root: &Path,
) -> NgcResult<LinkerStats> {
    let mut files_scanned = 0;
    let mut files_linked = 0;

    // Collect paths that need linking (can't mutate while iterating)
    let paths_to_link: Vec<PathBuf> = modules
        .iter()
        .filter(|(path, source)| {
            if !is_npm_module(path) {
                return false;
            }
            files_scanned += 1;
            needs_linking(source)
        })
        .map(|(path, _)| path.clone())
        .collect();

    for path in paths_to_link {
        if let Some(source) = modules.get(&path) {
            let source = source.clone();
            match transform::link_source(&source, &path)? {
                Some(linked) => {
                    modules.insert(path.clone(), linked);
                    files_linked += 1;
                    tracing::debug!(path = %path.display(), "linked Angular declarations");
                }
                None => {
                    // Detection said yes but transform found nothing — shouldn't happen
                    // but harmless
                }
            }
        }
    }

    Ok(LinkerStats {
        files_scanned,
        files_linked,
    })
}

/// Check whether a path is inside node_modules.
fn is_npm_module(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
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
        let stats = link_npm_modules(&mut modules, Path::new("/project")).unwrap();
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
        let stats = link_npm_modules(&mut modules, Path::new("/project")).unwrap();
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

        let stats = link_npm_modules(&mut modules, Path::new("/project")).unwrap();
        assert_eq!(stats.files_linked, 1);

        let linked = modules
            .get(Path::new("/project/node_modules/@test/pkg/index.mjs"))
            .unwrap();
        assert!(!linked.contains("\u{0275}\u{0275}ngDeclare"));
        assert!(linked.contains("_Factory"));
        assert!(linked.contains("\u{0275}\u{0275}defineInjectable"));
    }
}
