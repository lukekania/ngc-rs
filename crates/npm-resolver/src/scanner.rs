//! Import scanner for JavaScript/MJS files in npm packages.
//!
//! Uses regex to extract import specifiers from ESM source code,
//! similar to the project-resolver's import scanner.

use regex::Regex;
use std::sync::LazyLock;

/// A scanned import from an npm package file.
#[derive(Debug, Clone)]
pub struct ScannedNpmImport {
    /// The raw import specifier (e.g., `"./chunk.mjs"`, `"@angular/core"`).
    pub specifier: String,
    /// Whether this is a dynamic `import()` expression.
    pub is_dynamic: bool,
}

static FROM_RE: LazyLock<Regex> = LazyLock::new(|| {
    // [^;]*? matches any character except semicolons (including newlines in
    // Rust regex) so multi-line imports are detected without crossing
    // statement boundaries:
    //   import {
    //     isLeapYearIndex,
    //   } from "../utils.js";
    Regex::new(r#"(?:import|export)\s+[^;]*?\s+from\s+['"]([^'"]+)['"]"#).expect("valid regex")
});

static SIDE_EFFECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*import\s+['"]([^'"]+)['"]"#).expect("valid regex"));

static DYNAMIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"import\(\s*['"]([^'"]+)['"]\s*\)"#).expect("valid regex"));

static REEXPORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"export\s+\*\s+from\s+['"]([^'"]+)['"]"#).expect("valid regex"));

/// Scan a JavaScript/MJS source file for import specifiers.
///
/// Extracts specifiers from:
/// - `import { x } from 'specifier'`
/// - `import 'specifier'` (side-effect)
/// - `export { x } from 'specifier'`
/// - `export * from 'specifier'`
/// - `import('specifier')` (dynamic)
pub fn scan_npm_imports(source: &str) -> Vec<ScannedNpmImport> {
    let mut imports = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for cap in FROM_RE.captures_iter(source) {
        let spec = cap[1].to_string();
        if seen.insert((spec.clone(), false)) {
            imports.push(ScannedNpmImport {
                specifier: spec,
                is_dynamic: false,
            });
        }
    }

    for cap in SIDE_EFFECT_RE.captures_iter(source) {
        let spec = cap[1].to_string();
        if seen.insert((spec.clone(), false)) {
            imports.push(ScannedNpmImport {
                specifier: spec,
                is_dynamic: false,
            });
        }
    }

    for cap in REEXPORT_RE.captures_iter(source) {
        let spec = cap[1].to_string();
        if seen.insert((spec.clone(), false)) {
            imports.push(ScannedNpmImport {
                specifier: spec,
                is_dynamic: false,
            });
        }
    }

    for cap in DYNAMIC_RE.captures_iter(source) {
        let spec = cap[1].to_string();
        if seen.insert((spec.clone(), true)) {
            imports.push(ScannedNpmImport {
                specifier: spec,
                is_dynamic: true,
            });
        }
    }

    imports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_named_import() {
        let source = r#"import { Component } from '@angular/core';"#;
        let imports = scan_npm_imports(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "@angular/core");
        assert!(!imports[0].is_dynamic);
    }

    #[test]
    fn test_scan_side_effect_import() {
        let source = r#"import 'zone.js';"#;
        let imports = scan_npm_imports(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "zone.js");
    }

    #[test]
    fn test_scan_reexport() {
        let source = r#"export * from './internal/operators';"#;
        let imports = scan_npm_imports(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "./internal/operators");
    }

    #[test]
    fn test_scan_dynamic_import() {
        let source = r#"const m = import('./lazy-chunk.mjs');"#;
        let imports = scan_npm_imports(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "./lazy-chunk.mjs");
        assert!(imports[0].is_dynamic);
    }

    #[test]
    fn test_scan_mixed_imports() {
        let source = r#"
import { Component } from '@angular/core';
import { Router } from '@angular/router';
import './polyfill.js';
export { something } from './internal';
export * from './reexports';
const lazy = import('./chunk.mjs');
"#;
        let imports = scan_npm_imports(source);
        let specifiers: Vec<&str> = imports.iter().map(|i| i.specifier.as_str()).collect();
        assert!(specifiers.contains(&"@angular/core"));
        assert!(specifiers.contains(&"@angular/router"));
        assert!(specifiers.contains(&"./polyfill.js"));
        assert!(specifiers.contains(&"./internal"));
        assert!(specifiers.contains(&"./reexports"));
        assert!(specifiers.contains(&"./chunk.mjs"));
    }

    #[test]
    fn test_scan_deduplication() {
        let source = r#"
import { A } from '@angular/core';
import { B } from '@angular/core';
"#;
        let imports = scan_npm_imports(source);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].specifier, "@angular/core");
    }
}
