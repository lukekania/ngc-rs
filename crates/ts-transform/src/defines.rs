//! Build-time identifier substitution.
//!
//! Replaces references to a fixed set of identifiers (e.g. `ngDevMode`) with
//! literal expressions (e.g. `false`) at the source-text level, so a downstream
//! minifier can constant-fold and dead-code-eliminate the dev-only branches
//! that gate on those flags. Mirrors esbuild's `--define:` flag.
//!
//! Replacements only apply when the identifier resolves to the global scope —
//! a local `let ngDevMode`, a parameter, or a named import shadowing the name
//! is left untouched. Property access like `obj.ngDevMode` is also untouched
//! because the AST classifies the property as an `IdentifierName`, not an
//! `IdentifierReference`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::IdentifierReference;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_semantic::{IsGlobalReference, Scoping, SemanticBuilder};
use oxc_span::SourceType;
use rayon::prelude::*;

/// Map of identifier name → replacement source text.
///
/// Values are raw JavaScript source fragments (typically literals like
/// `"false"` or `"\"https://api.example.com\""`) spliced verbatim into the
/// output. The strings are owned so the map can carry user-supplied values
/// loaded from `angular.json` at runtime, not just `&'static` built-ins.
#[derive(Debug, Clone, Default)]
pub struct DefineMap {
    entries: HashMap<String, String>,
}

impl DefineMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a single replacement, overwriting any existing entry for `name`.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.insert(name.into(), value.into());
    }

    /// Look up the replacement source text for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(String::as_str)
    }

    /// Whether the map has no entries (used to short-circuit the substitution
    /// pass entirely on no-op runs).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Build a [`DefineMap`] from an arbitrary `name → value` map. Values are
    /// raw JS source fragments — pass `"42"`, `"true"`, `"\"https://x\""`, or
    /// `"{\"k\":1}"` rather than the unquoted Rust strings.
    pub fn from_map<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map = Self::new();
        for (k, v) in entries {
            map.insert(k, v);
        }
        map
    }

    /// Merge `other` into `self`. Entries in `other` overwrite existing
    /// entries with the same key; collisions are reported via
    /// [`tracing::warn`] so an `angular.json` define that shadows one of
    /// ngc-rs's built-in flags (`ngDevMode`, `ngI18nClosureMode`,
    /// `ngJitMode`) is visible in the build log.
    pub fn merge_overriding(&mut self, other: DefineMap) {
        for (key, value) in other.entries {
            if let Some(existing) = self.entries.get(&key) {
                if existing != &value {
                    tracing::warn!(
                        define = %key,
                        builtin = %existing,
                        user = %value,
                        "user-supplied `define` overrides ngc-rs built-in flag"
                    );
                }
            }
            self.entries.insert(key, value);
        }
    }

    /// Defines applied to Angular production builds. Lowering these to literal
    /// `false` lets the bundler/minifier eliminate every `if (ngDevMode) { … }`
    /// branch in `@angular/core` and friends.
    pub fn production_angular() -> Self {
        let mut map = Self::new();
        map.insert("ngDevMode", "false");
        map.insert("ngI18nClosureMode", "false");
        map.insert("ngJitMode", "false");
        map
    }
}

/// Apply `defines` to `source`, returning the substituted source text.
///
/// On parse failure or when the map is empty, returns the input unchanged —
/// this pass is a best-effort optimisation, not a correctness gate.
pub fn apply_defines(source: &str, file_name: &str, defines: &DefineMap) -> String {
    if defines.is_empty() {
        return source.to_string();
    }

    let allocator = Allocator::new();
    let path = Path::new(file_name);
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx());

    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return source.to_string();
    }

    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
    let scoping = semantic.scoping();

    let mut collector = Collector::new(scoping, defines);
    collector.visit_program(&parsed.program);

    let mut replacements = collector.replacements;
    if replacements.is_empty() {
        return source.to_string();
    }
    replacements.sort_unstable_by_key(|(start, _, _)| *start);

    let mut out = String::with_capacity(source.len());
    let mut cursor: usize = 0;
    for (start, end, replacement) in replacements {
        let start = start as usize;
        let end = end as usize;
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(replacement);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

/// Apply `defines` to every module in `modules` in parallel.
///
/// Each module is keyed by its canonical path; the path's extension drives
/// the parser's source-type detection. The map is updated in place.
pub fn apply_defines_to_modules(modules: &mut HashMap<PathBuf, String>, defines: &DefineMap) {
    if defines.is_empty() {
        return;
    }
    let updates: Vec<(PathBuf, String)> = modules
        .par_iter()
        .filter_map(|(path, code)| {
            let file_name = path.to_string_lossy();
            let next = apply_defines(code, file_name.as_ref(), defines);
            if next == *code {
                None
            } else {
                Some((path.clone(), next))
            }
        })
        .collect();
    for (path, code) in updates {
        modules.insert(path, code);
    }
}

struct Collector<'a, 'b> {
    scoping: &'b Scoping,
    defines: &'b DefineMap,
    replacements: Vec<(u32, u32, &'b str)>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a, 'b> Collector<'a, 'b> {
    fn new(scoping: &'b Scoping, defines: &'b DefineMap) -> Self {
        Self {
            scoping,
            defines,
            replacements: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'a, 'b> Visit<'a> for Collector<'a, 'b> {
    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        let Some(replacement) = self.defines.get(it.name.as_str()) else {
            return;
        };
        if !it.is_global_reference(self.scoping) {
            return;
        }
        // Skip writes — `ngDevMode = something` would become `false = something`,
        // a syntax error. These flags are read-only in practice, but defending
        // against the edge case keeps the pass safe.
        if let Some(reference_id) = it.reference_id.get() {
            if self.scoping.get_reference(reference_id).is_write() {
                return;
            }
        }
        self.replacements
            .push((it.span.start, it.span.end, replacement));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defines() -> DefineMap {
        DefineMap::production_angular()
    }

    #[test]
    fn replaces_global_identifier_reference() {
        let src = "if (ngDevMode) { console.log('dev'); }\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, "if (false) { console.log('dev'); }\n");
    }

    #[test]
    fn replaces_typeof_call_site() {
        // `typeof ngDevMode !== 'undefined' && ngDevMode` is the actual guard
        // pattern in @angular/core. After substitution, both reads become `false`,
        // letting the minifier collapse the whole expression.
        let src = "const x = typeof ngDevMode !== 'undefined' && ngDevMode;\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, "const x = typeof false !== 'undefined' && false;\n");
    }

    #[test]
    fn replaces_all_three_angular_flags() {
        let src = "[ngDevMode, ngI18nClosureMode, ngJitMode];\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, "[false, false, false];\n");
    }

    #[test]
    fn preserves_property_access() {
        let src = "obj.ngDevMode;\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn preserves_property_with_global_object() {
        // `globalThis.ngDevMode` must NOT be touched: the property name is an
        // IdentifierName, not an IdentifierReference.
        let src = "globalThis.ngDevMode = false;\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn shadowed_by_const_is_kept() {
        let src = "{ const ngDevMode = 1; console.log(ngDevMode); }\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn shadowed_by_let_is_kept() {
        let src = "function f() { let ngDevMode = 0; return ngDevMode; }\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn shadowed_by_var_is_kept() {
        let src = "function f() { var ngDevMode = 0; return ngDevMode; }\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn shadowed_by_parameter_is_kept() {
        let src = "function f(ngDevMode) { return ngDevMode; }\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn shadowed_by_named_import_is_kept() {
        let src = "import { ngDevMode } from './x';\nconsole.log(ngDevMode);\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn shadowed_by_renamed_import_is_kept() {
        // `import { foo as ngDevMode }` creates a local binding called
        // `ngDevMode` that shadows the global.
        let src = "import { foo as ngDevMode } from './x';\nconsole.log(ngDevMode);\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn outer_scope_replaced_inner_shadow_kept() {
        // The outer `ngDevMode` is a global reference and must be replaced;
        // the inner `ngDevMode` is bound by `const` and must be kept.
        let src = "console.log(ngDevMode);\n{ const ngDevMode = 1; console.log(ngDevMode); }\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(
            out,
            "console.log(false);\n{ const ngDevMode = 1; console.log(ngDevMode); }\n"
        );
    }

    #[test]
    fn assignment_target_is_not_replaced() {
        // We don't replace writes — `false = x` would be a syntax error.
        let src = "ngDevMode = true;\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn empty_define_map_returns_input() {
        let src = "if (ngDevMode) {}\n";
        let out = apply_defines(src, "in.js", &DefineMap::new());
        assert_eq!(out, src);
    }

    #[test]
    fn unrelated_identifier_is_untouched() {
        let src = "const someOtherFlag = true;\n";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn unparseable_source_returns_input_unchanged() {
        let src = "if (ngDevMode { )) {{{";
        let out = apply_defines(src, "in.js", &defines());
        assert_eq!(out, src);
    }

    #[test]
    fn collector_constructor_initialises_empty() {
        let allocator = Allocator::new();
        let parsed = Parser::new(&allocator, "", SourceType::mjs()).parse();
        let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
        let defines = defines();
        let c = Collector::new(semantic.scoping(), &defines);
        assert!(c.replacements.is_empty());
    }

    // ----- User-supplied define support (issue #137) -----

    #[test]
    fn user_string_literal_value_is_substituted_verbatim() {
        // `angular.json` value is a JSON string: `"\"https://api.example.com\""`.
        // The outer quotes are part of the replacement source text so the
        // resulting JS contains a string literal.
        let map = DefineMap::from_map([("__APP_API_URL__", "\"https://api.example.com\"")]);
        let src = "console.log(__APP_API_URL__);\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "console.log(\"https://api.example.com\");\n");
    }

    #[test]
    fn user_number_value_is_substituted_verbatim() {
        let map = DefineMap::from_map([("__BUILD_NUMBER__", "42")]);
        let src = "const n = __BUILD_NUMBER__;\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "const n = 42;\n");
    }

    #[test]
    fn user_boolean_value_is_substituted_verbatim() {
        let map = DefineMap::from_map([("__FEATURE_X__", "true")]);
        let src = "if (__FEATURE_X__) {}\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "if (true) {}\n");
    }

    #[test]
    fn user_json_object_value_is_substituted_verbatim() {
        let map = DefineMap::from_map([("__CFG__", "{\"k\":1}")]);
        let src = "const c = __CFG__;\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "const c = {\"k\":1};\n");
    }

    #[test]
    fn merge_overriding_user_value_wins() {
        // User's `ngDevMode` (= `true`) overrides the production_angular
        // built-in (= `false`). The collision triggers a tracing warning,
        // but here we just assert the resulting substitution.
        let mut map = DefineMap::production_angular();
        map.merge_overriding(DefineMap::from_map([("ngDevMode", "true")]));
        let src = "if (ngDevMode) { 'a'; }\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "if (true) { 'a'; }\n");
    }

    #[test]
    fn merge_overriding_keeps_unrelated_builtins() {
        let mut map = DefineMap::production_angular();
        map.merge_overriding(DefineMap::from_map([("__FOO__", "1")]));
        // Built-ins still in place.
        let src = "if (ngI18nClosureMode) {}\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "if (false) {}\n");
        // User entry took effect too.
        let src = "const f = __FOO__;\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "const f = 1;\n");
    }

    #[test]
    fn merge_overriding_same_value_is_a_noop() {
        // Re-defining a built-in to the same value shouldn't change behaviour;
        // (and the warning path checks `existing != value`, so it stays quiet).
        let mut map = DefineMap::production_angular();
        map.merge_overriding(DefineMap::from_map([("ngDevMode", "false")]));
        let src = "if (ngDevMode) {}\n";
        let out = apply_defines(src, "in.js", &map);
        assert_eq!(out, "if (false) {}\n");
    }

    #[test]
    fn from_map_empty_iterator_is_empty() {
        let map = DefineMap::from_map(std::iter::empty::<(String, String)>());
        assert!(map.is_empty());
    }

    #[test]
    fn len_reports_entry_count() {
        let mut map = DefineMap::new();
        assert_eq!(map.len(), 0);
        map.insert("A", "1");
        map.insert("B", "2");
        assert_eq!(map.len(), 2);
    }
}
