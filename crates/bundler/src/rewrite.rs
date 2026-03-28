use std::collections::BTreeSet;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{ExportDefaultDeclarationKind, ModuleDeclaration};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// A module after import/export rewriting for bundling.
#[derive(Debug, Clone)]
pub struct RewrittenModule {
    /// The module code with local imports and exports stripped.
    pub code: String,
    /// External imports collected from this module.
    pub external_imports: Vec<ExternalImport>,
}

/// An external import that should be hoisted to the top of the bundle.
#[derive(Debug, Clone)]
pub struct ExternalImport {
    /// The import source (e.g. `@angular/core`).
    pub source: String,
    /// The default import binding, if any (e.g. `_decorate`).
    pub default_import: Option<String>,
    /// Named import bindings (e.g. `Component`, `RouterOutlet`).
    pub named_imports: BTreeSet<String>,
    /// Whether this is a side-effect-only import (`import 'zone.js'`).
    pub is_side_effect: bool,
}

/// A span range to remove from the source text.
struct Removal {
    start: u32,
    end: u32,
}

/// Rewrite a single JavaScript module for bundling.
///
/// Parses the JS code, classifies each import as local or external based on
/// `local_prefixes`, strips local imports and export keywords, and collects
/// external imports for hoisting.
pub fn rewrite_module(
    js_code: &str,
    file_name: &str,
    local_prefixes: &[&str],
) -> NgcResult<RewrittenModule> {
    let allocator = Allocator::new();
    let source_type = SourceType::mjs();
    let parsed = Parser::new(&allocator, js_code, source_type).parse();

    if parsed.panicked {
        return Err(NgcError::BundleError {
            message: format!("failed to parse {file_name} for bundling"),
        });
    }

    let mut removals: Vec<Removal> = Vec::new();
    let mut external_imports: Vec<ExternalImport> = Vec::new();

    for stmt in &parsed.program.body {
        let module_decl = match stmt.as_module_declaration() {
            Some(decl) => decl,
            None => continue,
        };

        match module_decl {
            ModuleDeclaration::ImportDeclaration(import) => {
                let source = import.source.value.as_str();
                if is_local(source, local_prefixes) {
                    removals.push(Removal {
                        start: import.span.start,
                        end: import.span.end,
                    });
                } else {
                    let mut named = BTreeSet::new();
                    let mut default = None;
                    let mut is_side_effect = true;

                    if let Some(specifiers) = &import.specifiers {
                        for spec in specifiers {
                            is_side_effect = false;
                            match spec {
                                oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                    named.insert(s.local.name.to_string());
                                }
                                oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(
                                    s,
                                ) => {
                                    default = Some(s.local.name.to_string());
                                }
                                oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(
                                    s,
                                ) => {
                                    named.insert(format!("* as {}", s.local.name));
                                }
                            }
                        }
                    }

                    external_imports.push(ExternalImport {
                        source: source.to_string(),
                        default_import: default,
                        named_imports: named,
                        is_side_effect,
                    });
                    removals.push(Removal {
                        start: import.span.start,
                        end: import.span.end,
                    });
                }
            }
            ModuleDeclaration::ExportNamedDeclaration(export) => {
                if export.source.is_some() {
                    // Re-export: `export { Foo } from './foo'` — remove entirely
                    removals.push(Removal {
                        start: export.span.start,
                        end: export.span.end,
                    });
                } else if export.declaration.is_some() {
                    // `export class Foo` or `export const x` — remove only `export` keyword
                    removals.push(Removal {
                        start: export.span.start,
                        end: export.span.start + 7, // "export "
                    });
                } else {
                    // `export { Foo, Bar }` — export list, remove entirely
                    removals.push(Removal {
                        start: export.span.start,
                        end: export.span.end,
                    });
                }
            }
            ModuleDeclaration::ExportDefaultDeclaration(export) => {
                match &export.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(_)
                    | ExportDefaultDeclarationKind::ClassDeclaration(_) => {
                        // `export default class Foo` — remove `export default`
                        removals.push(Removal {
                            start: export.span.start,
                            end: export.span.start + 15, // "export default "
                        });
                    }
                    _ => {
                        // `export default expr` — remove entirely for now
                        removals.push(Removal {
                            start: export.span.start,
                            end: export.span.end,
                        });
                    }
                }
            }
            ModuleDeclaration::ExportAllDeclaration(export) => {
                if is_local(export.source.value.as_str(), local_prefixes) {
                    removals.push(Removal {
                        start: export.span.start,
                        end: export.span.end,
                    });
                } else {
                    // External re-export — keep as-is for now
                }
            }
            _ => {}
        }
    }

    let code = apply_removals(js_code, &mut removals);

    Ok(RewrittenModule {
        code,
        external_imports,
    })
}

/// Check if an import specifier is local based on known prefixes.
fn is_local(specifier: &str, local_prefixes: &[&str]) -> bool {
    local_prefixes
        .iter()
        .any(|prefix| specifier.starts_with(prefix))
}

/// Apply span removals to the source text, producing the rewritten code.
fn apply_removals(source: &str, removals: &mut [Removal]) -> String {
    // Sort in reverse order so later removals don't shift earlier offsets
    removals.sort_by(|a, b| b.start.cmp(&a.start));

    let mut result = source.to_string();
    for removal in removals.iter() {
        let start = removal.start as usize;
        let end = removal.end as usize;
        if start <= result.len() && end <= result.len() {
            // Also remove trailing newline if present
            let actual_end = if end < result.len() && result.as_bytes()[end] == b'\n' {
                end + 1
            } else {
                end
            };
            result.replace_range(start..actual_end, "");
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_external_named_import_collected_and_removed() {
        let code = "import { Component } from '@angular/core';\nclass Foo {}\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert!(!result.code.contains("import"));
        assert_eq!(result.external_imports.len(), 1);
        assert_eq!(result.external_imports[0].source, "@angular/core");
        assert!(result.external_imports[0]
            .named_imports
            .contains("Component"));
    }

    #[test]
    fn test_external_default_import_collected() {
        let code = "import _decorate from '@oxc-project/runtime/helpers/decorate';\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert_eq!(
            result.external_imports[0].default_import,
            Some("_decorate".to_string())
        );
    }

    #[test]
    fn test_local_relative_import_removed() {
        let code = "import { Foo } from './foo';\nconst x = 1;\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert!(!result.code.contains("import"));
        assert!(result.code.contains("const x = 1"));
        assert!(result.external_imports.is_empty());
    }

    #[test]
    fn test_local_alias_import_removed() {
        let code = "import { SharedUtils } from '@app/shared';\n";
        let result = rewrite_module(code, "test.js", &[".", "@app/"]).expect("should rewrite");
        assert!(!result.code.contains("import"));
        assert!(result.external_imports.is_empty());
    }

    #[test]
    fn test_reexport_removed() {
        let code = "export { SharedUtils } from './utils';\nexport { Logger } from './logger';\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert!(!result.code.contains("export"));
        assert!(!result.code.contains("SharedUtils"));
    }

    #[test]
    fn test_export_class_keyword_stripped() {
        let code = "export class Logger {\n\tstatic log(msg) {}\n}\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert!(result.code.contains("class Logger"));
        assert!(!result.code.contains("export"));
    }

    #[test]
    fn test_export_const_keyword_stripped() {
        let code = "export const routes = [];\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert!(result.code.contains("const routes = []"));
        assert!(!result.code.contains("export"));
    }

    #[test]
    fn test_export_list_removed() {
        let code = "let AppComponent = class AppComponent {};\nexport { AppComponent };\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert!(result.code.contains("let AppComponent"));
        assert!(!result.code.contains("export"));
    }

    #[test]
    fn test_side_effect_external_import() {
        let code = "import 'zone.js';\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert_eq!(result.external_imports.len(), 1);
        assert!(result.external_imports[0].is_side_effect);
        assert_eq!(result.external_imports[0].source, "zone.js");
    }

    #[test]
    fn test_module_with_no_imports() {
        let code = "const x = 42;\n";
        let result = rewrite_module(code, "test.js", &["."]).expect("should rewrite");
        assert_eq!(result.code.trim(), "const x = 42;");
        assert!(result.external_imports.is_empty());
    }
}
