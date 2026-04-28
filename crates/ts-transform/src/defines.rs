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
/// `"false"`) spliced verbatim into the output.
#[derive(Debug, Clone, Default)]
pub struct DefineMap {
    entries: HashMap<String, &'static str>,
}

impl DefineMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, value: &'static str) {
        self.entries.insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> Option<&'static str> {
        self.entries.get(name).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
    replacements: Vec<(u32, u32, &'static str)>,
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

impl<'a> Visit<'a> for Collector<'a, '_> {
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
}
