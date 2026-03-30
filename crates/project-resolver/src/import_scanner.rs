use regex::Regex;
use std::sync::LazyLock;

/// Matches `from '...'` or `from "..."` in import/export statements.
/// Handles multi-line imports naturally since we only look for the from-clause.
static FROM_CLAUSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)from\s+['"]([^'"]+)['"]"#).expect("FROM_CLAUSE_RE is a valid regex")
});

/// Matches side-effect imports: `import './polyfills'` or `import "./polyfills"`.
static SIDE_EFFECT_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*import\s+['"]([^'"]+)['"]"#)
        .expect("SIDE_EFFECT_IMPORT_RE is a valid regex")
});

/// Matches dynamic imports: `import('./lazy')` or `import("./lazy")`.
static DYNAMIC_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"import\(\s*['"]([^'"]+)['"]\s*\)"#).expect("DYNAMIC_IMPORT_RE is a valid regex")
});

/// Distinguishes static `import`/`export` declarations from dynamic `import()` expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImportKind {
    /// A static `import ... from '...'`, `export ... from '...'`, or side-effect `import '...'`.
    Static,
    /// A dynamic `import('...')` expression (triggers code splitting).
    Dynamic,
}

/// An import specifier with its kind (static or dynamic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedImport {
    /// The raw import specifier string.
    pub specifier: String,
    /// Whether this is a static or dynamic import.
    pub kind: ImportKind,
}

/// Scan a TypeScript source file and extract all import specifiers with their kind.
///
/// Extracts specifiers from:
/// - Static `import ... from '...'` statements → [`ImportKind::Static`]
/// - `export ... from '...'` re-exports → [`ImportKind::Static`]
/// - Side-effect `import '...'` statements → [`ImportKind::Static`]
/// - Dynamic `import('...')` expressions → [`ImportKind::Dynamic`]
///
/// If the same specifier appears as both static and dynamic, both entries are emitted.
/// Within each kind, specifiers are deduplicated.
pub fn scan_imports_with_kind(source: &str) -> Vec<ScannedImport> {
    let mut imports = Vec::new();
    let mut seen_static = std::collections::HashSet::new();
    let mut seen_dynamic = std::collections::HashSet::new();

    for cap in FROM_CLAUSE_RE.captures_iter(source) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str().to_string();
            if seen_static.insert(s.clone()) {
                imports.push(ScannedImport {
                    specifier: s,
                    kind: ImportKind::Static,
                });
            }
        }
    }

    for cap in SIDE_EFFECT_IMPORT_RE.captures_iter(source) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str().to_string();
            if seen_static.insert(s.clone()) {
                imports.push(ScannedImport {
                    specifier: s,
                    kind: ImportKind::Static,
                });
            }
        }
    }

    for cap in DYNAMIC_IMPORT_RE.captures_iter(source) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str().to_string();
            if seen_dynamic.insert(s.clone()) {
                imports.push(ScannedImport {
                    specifier: s,
                    kind: ImportKind::Dynamic,
                });
            }
        }
    }

    imports
}

/// Scan a TypeScript source file's contents and extract all import specifiers.
///
/// Extracts specifiers from:
/// - Static `import ... from '...'` statements
/// - `export ... from '...'` re-exports
/// - Side-effect `import '...'` statements
/// - Dynamic `import('...')` expressions
///
/// Returns raw specifier strings. Classification (relative vs alias vs bare)
/// happens during graph construction where tsconfig context is available.
pub fn scan_imports(source: &str) -> Vec<String> {
    let mut specifiers = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for imp in scan_imports_with_kind(source) {
        if seen.insert(imp.specifier.clone()) {
            specifiers.push(imp.specifier);
        }
    }

    specifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_named_import() {
        let source = r#"import { Component } from '@angular/core';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["@angular/core"]);
    }

    #[test]
    fn test_static_default_import() {
        let source = r#"import AppComponent from './app.component';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./app.component"]);
    }

    #[test]
    fn test_static_namespace_import() {
        let source = r#"import * as utils from './utils';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./utils"]);
    }

    #[test]
    fn test_reexport() {
        let source = r#"export { SharedUtils } from './utils';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./utils"]);
    }

    #[test]
    fn test_star_reexport() {
        let source = r#"export * from './logger';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./logger"]);
    }

    #[test]
    fn test_side_effect_import() {
        let source = r#"import './polyfills';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./polyfills"]);
    }

    #[test]
    fn test_dynamic_import() {
        let source = r#"const module = import('./lazy-module');"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./lazy-module"]);
    }

    #[test]
    fn test_multi_line_import() {
        let source = r#"import {
  Component,
  OnInit,
  Injectable
} from '@angular/core';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["@angular/core"]);
    }

    #[test]
    fn test_multiple_imports() {
        let source = r#"import { Component } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import { SharedUtils } from '@app/shared';"#;
        let imports = scan_imports(source);
        assert_eq!(
            imports,
            vec!["@angular/core", "@angular/router", "@app/shared"]
        );
    }

    #[test]
    fn test_type_import() {
        let source = r#"import type { Routes } from '@angular/router';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["@angular/router"]);
    }

    #[test]
    fn test_deduplication() {
        let source = r#"import { Component } from '@angular/core';
import { Injectable } from '@angular/core';"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["@angular/core"]);
    }

    #[test]
    fn test_mixed_quotes() {
        let source = r#"import { Foo } from './foo';
import { Bar } from "./bar";"#;
        let imports = scan_imports(source);
        assert_eq!(imports, vec!["./foo", "./bar"]);
    }

    #[test]
    fn test_empty_source() {
        let imports = scan_imports("");
        assert!(imports.is_empty());
    }

    #[test]
    fn test_no_imports() {
        let source = "const x = 42;\nconsole.log(x);";
        let imports = scan_imports(source);
        assert!(imports.is_empty());
    }

    #[test]
    fn test_scan_with_kind_static() {
        let source = r#"import { Component } from '@angular/core';"#;
        let imports = scan_imports_with_kind(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "@angular/core");
        assert_eq!(imports[0].kind, ImportKind::Static);
    }

    #[test]
    fn test_scan_with_kind_dynamic() {
        let source = r#"const m = import('./lazy-module');"#;
        let imports = scan_imports_with_kind(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "./lazy-module");
        assert_eq!(imports[0].kind, ImportKind::Dynamic);
    }

    #[test]
    fn test_scan_with_kind_mixed() {
        let source = r#"import { Routes } from '@angular/router';
const routes = [
  { path: 'admin', loadComponent: () => import('./admin/admin.component').then(m => m.AdminComponent) },
];"#;
        let imports = scan_imports_with_kind(source);
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].specifier, "@angular/router");
        assert_eq!(imports[0].kind, ImportKind::Static);
        assert_eq!(imports[1].specifier, "./admin/admin.component");
        assert_eq!(imports[1].kind, ImportKind::Dynamic);
    }

    #[test]
    fn test_scan_with_kind_same_specifier_both_kinds() {
        let source = r#"import { Foo } from './foo';
const lazy = import('./foo');"#;
        let imports = scan_imports_with_kind(source);
        assert_eq!(imports.len(), 2);
        assert!(imports
            .iter()
            .any(|i| i.specifier == "./foo" && i.kind == ImportKind::Static));
        assert!(imports
            .iter()
            .any(|i| i.specifier == "./foo" && i.kind == ImportKind::Dynamic));
    }
}
