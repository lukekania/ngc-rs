//! Registry of NgModule classes and their exported directive/pipe/component lists.
//!
//! Populated from both npm `ɵɵngDeclareNgModule` calls (during npm linking) and
//! `ɵɵdefineNgModule` calls (project AOT output). Used by the post-link pass that
//! flattens standalone-component `imports` arrays at build time, mirroring what
//! `ng build` does.
//!
//! ## Why
//!
//! Modern Angular apps are standalone-first, but npm packages still ship their
//! directives bundled into NgModules (e.g. `ReactiveFormsModule` from
//! `@angular/forms`). When a standalone component does
//! `imports: [ReactiveFormsModule]`, Angular's official compiler walks the
//! module's `ɵmod.exports` transitively and emits a flat directive class list
//! on the component def. Doing this at compile time avoids the runtime
//! `ɵɵgetComponentDepsFactory` helper, which has known instantiation-order bugs
//! that surface as `NG01050` for reactive forms inside dialogs.
//!
//! ## Lifecycle
//!
//! - **Population is split** between npm linking (`ng_module::transform`) and a
//!   post-pass that scans every module for `ɵɵdefineNgModule(`. This is why
//!   transitive flattening is *lazy* — both populations must complete before
//!   any flatten lookup is correct.
//! - **Per-invocation** today; watch mode (v0.8) will need invalidation hooks.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

/// Maps NgModule class names to their (transitively flattened) exported
/// directive/pipe/component class names.
///
/// All keys/values are local source identifiers as they appear in the file
/// where the NgModule was declared. Cross-file linking happens via the bundler
/// after this stage.
#[derive(Debug, Default)]
pub struct ModuleRegistry {
    /// Class name -> direct exports (in source order, possibly containing other
    /// module names that need transitive expansion).
    raw_exports: RwLock<HashMap<String, Vec<String>>>,
    /// Set of class names known to be NgModules (i.e., have been registered).
    is_module: RwLock<HashSet<String>>,
    /// Memoized result of [`flatten`]. Cleared implicitly between builds since
    /// each build constructs a fresh registry.
    flat_cache: RwLock<HashMap<String, Vec<String>>>,
}

impl ModuleRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an NgModule with its direct (un-flattened) exports list.
    ///
    /// Subsequent registrations for the same module name overwrite the previous
    /// entry and emit a `tracing::warn!`. This handles the rare case where a
    /// project file declares an NgModule with the same name as one in an npm
    /// package.
    pub fn register(&self, name: &str, exports: Vec<String>) {
        {
            let mut raw = match self.raw_exports.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if raw.contains_key(name) {
                tracing::warn!(
                    module = name,
                    "NgModule registered more than once; later registration wins"
                );
            }
            raw.insert(name.to_string(), exports);
        }
        {
            let mut set = match self.is_module.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            set.insert(name.to_string());
        }
        if let Ok(mut cache) = self.flat_cache.write() {
            cache.clear();
        }
    }

    /// Whether the given class name is a known NgModule.
    pub fn is_module(&self, name: &str) -> bool {
        match self.is_module.read() {
            Ok(g) => g.contains(name),
            Err(p) => p.into_inner().contains(name),
        }
    }

    /// Number of registered NgModules.
    pub fn len(&self) -> usize {
        match self.is_module.read() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return the transitively flattened directive/pipe class list for `name`.
    ///
    /// If `name` is not a known NgModule, returns `vec![name]` (pass-through —
    /// the caller is treating the identifier as a possible module reference but
    /// it might be a plain directive).
    ///
    /// Cycle-safe: a `visited` set prevents infinite recursion when
    /// pathological NgModules transitively re-export each other.
    /// Order-preserving: depth-first source order with O(n) dedup. The lists
    /// are small (typically <30) so the linear contains-check is fine.
    pub fn flatten(&self, name: &str) -> Vec<String> {
        if let Ok(cache) = self.flat_cache.read() {
            if let Some(hit) = cache.get(name) {
                return hit.clone();
            }
        }
        let mut visited = HashSet::new();
        let mut out = Vec::new();
        self.walk(name, &mut visited, &mut out);
        if let Ok(mut cache) = self.flat_cache.write() {
            cache.insert(name.to_string(), out.clone());
        }
        out
    }

    fn walk(&self, name: &str, visited: &mut HashSet<String>, out: &mut Vec<String>) {
        if !visited.insert(name.to_string()) {
            return;
        }
        let direct = match self.raw_exports.read() {
            Ok(g) => g.get(name).cloned(),
            Err(p) => p.into_inner().get(name).cloned(),
        };
        match direct {
            Some(exports) => {
                for e in exports {
                    if self.is_module(&e) {
                        self.walk(&e, visited, out);
                    } else if !out.iter().any(|x| x == &e) {
                        out.push(e);
                    }
                }
            }
            None => {
                if !out.iter().any(|x| x == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
}

/// Parse an array-literal source text like `"[A, B, C]"` into a list of bare
/// identifier names. Returns `None` if the text is not a simple identifier list
/// (e.g. contains spreads, calls, or non-identifier elements).
///
/// Used by callers that have already extracted the raw source text of an
/// `exports`/`imports`/`declarations` array property via
/// [`crate::metadata::get_source_text`].
pub fn parse_identifier_array(src: &str) -> Option<Vec<String>> {
    let trimmed = src.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    for raw in inner.split(',') {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if !is_identifier(item) {
            return None;
        }
        out.push(item.to_string());
    }
    Some(out)
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_passthrough_for_unknown_class() {
        let reg = ModuleRegistry::new();
        assert_eq!(reg.flatten("FormControlName"), vec!["FormControlName"]);
    }

    #[test]
    fn flatten_returns_direct_exports() {
        let reg = ModuleRegistry::new();
        reg.register("MyModule", vec!["DirA".into(), "DirB".into()]);
        assert_eq!(reg.flatten("MyModule"), vec!["DirA", "DirB"]);
    }

    #[test]
    fn flatten_walks_transitive_modules_in_source_order() {
        let reg = ModuleRegistry::new();
        // ReactiveFormsModule shape: re-exports an internal shared module,
        // followed by its own directives.
        reg.register(
            "InternalShared",
            vec!["DefaultValueAccessor".into(), "NgControlStatus".into()],
        );
        reg.register(
            "ReactiveFormsModule",
            vec![
                "InternalShared".into(),
                "FormGroupDirective".into(),
                "FormControlName".into(),
            ],
        );
        let flat = reg.flatten("ReactiveFormsModule");
        assert_eq!(
            flat,
            vec![
                "DefaultValueAccessor",
                "NgControlStatus",
                "FormGroupDirective",
                "FormControlName"
            ]
        );
    }

    #[test]
    fn flatten_dedups_duplicate_directives_across_modules() {
        let reg = ModuleRegistry::new();
        reg.register("ModA", vec!["SharedDir".into(), "DirA".into()]);
        reg.register("ModB", vec!["SharedDir".into(), "DirB".into()]);
        reg.register("Combined", vec!["ModA".into(), "ModB".into()]);
        let flat = reg.flatten("Combined");
        assert_eq!(flat, vec!["SharedDir", "DirA", "DirB"]);
    }

    #[test]
    fn flatten_handles_cycles() {
        let reg = ModuleRegistry::new();
        reg.register("A", vec!["B".into(), "DirA".into()]);
        reg.register("B", vec!["A".into(), "DirB".into()]);
        let flat = reg.flatten("A");
        // Walks A -> B (skips A re-entry) -> DirB, then DirA.
        assert_eq!(flat, vec!["DirB", "DirA"]);
    }

    #[test]
    fn flatten_caches_result() {
        let reg = ModuleRegistry::new();
        reg.register("M", vec!["D".into()]);
        let first = reg.flatten("M");
        let second = reg.flatten("M");
        assert_eq!(first, second);
    }

    #[test]
    fn register_invalidates_flat_cache() {
        let reg = ModuleRegistry::new();
        reg.register("M", vec!["DirOld".into()]);
        let _ = reg.flatten("M");
        reg.register("M", vec!["DirNew".into()]);
        assert_eq!(reg.flatten("M"), vec!["DirNew"]);
    }

    #[test]
    fn is_module_reflects_registration() {
        let reg = ModuleRegistry::new();
        assert!(!reg.is_module("M"));
        reg.register("M", vec![]);
        assert!(reg.is_module("M"));
    }

    #[test]
    fn parse_identifier_array_basic() {
        assert_eq!(
            parse_identifier_array("[A, B, C]"),
            Some(vec!["A".into(), "B".into(), "C".into()])
        );
    }

    #[test]
    fn parse_identifier_array_handles_whitespace() {
        assert_eq!(
            parse_identifier_array("[\n  A,\n  B,\n]"),
            Some(vec!["A".into(), "B".into()])
        );
    }

    #[test]
    fn parse_identifier_array_rejects_non_identifier_elements() {
        assert!(parse_identifier_array("[a.b, c]").is_none());
        assert!(parse_identifier_array("[fn(), c]").is_none());
        assert!(parse_identifier_array("[...spread, c]").is_none());
    }

    #[test]
    fn parse_identifier_array_accepts_empty() {
        assert_eq!(parse_identifier_array("[]"), Some(vec![]));
    }

    #[test]
    fn parse_identifier_array_rejects_non_array() {
        assert!(parse_identifier_array("foo").is_none());
        assert!(parse_identifier_array("(a, b)").is_none());
    }
}
