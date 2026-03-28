use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, Decorator, Expression, ObjectPropertyKind, PropertyKey, Statement};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// Metadata extracted from an `@Component` decorator.
#[derive(Debug, Clone)]
pub struct ExtractedComponent {
    /// The class name (e.g. `AppComponent`).
    pub class_name: String,
    /// The component selector (e.g. `app-root`).
    pub selector: String,
    /// The inline template string, if present.
    pub template: Option<String>,
    /// The templateUrl, if present (triggers JIT fallback).
    pub template_url: Option<String>,
    /// Whether the component is standalone.
    pub standalone: bool,
    /// Raw source text of the imports array (e.g. `[RouterOutlet]`).
    pub imports_source: Option<String>,
    /// Individual import identifiers (e.g. `["RouterOutlet"]`).
    #[allow(dead_code)]
    pub imports_identifiers: Vec<String>,
    /// Byte offset range of the full decorator (from `@` to closing `)`).
    pub decorator_span: (u32, u32),
    /// Byte offset of the class body opening `{`.
    pub class_body_start: u32,
    /// Byte offset of the `export` keyword, if present.
    #[allow(dead_code)]
    pub export_keyword_start: Option<u32>,
    /// Byte offset of the class keyword.
    #[allow(dead_code)]
    pub class_keyword_start: u32,
    /// The source text of the `@angular/core` import declaration span, for rewriting.
    pub angular_core_import_span: Option<(u32, u32)>,
    /// Named imports from `@angular/core` other than `Component`.
    pub other_angular_core_imports: Vec<String>,
    /// Raw source text of the `styles` array (e.g. `['\`.sidebar { ... }\`']`).
    pub styles_source: Option<String>,
}

impl ExtractedComponent {
    /// Check whether the component should use JIT fallback.
    ///
    /// Returns true only for patterns not yet supported by the native compiler.
    /// Most constructs are now compiled natively: elements, bindings, events,
    /// @if/@for/@switch, pipes, ng-content, ng-container, ng-template,
    /// *ngIf/*ngFor, [(two-way)], #ref, and templateUrl.
    pub fn needs_jit_fallback(&self) -> bool {
        false
    }
}

/// Extract `@Component` metadata from a TypeScript source file.
///
/// Returns `None` if no `@Component` decorator is found. Returns an error
/// if the source can't be parsed.
pub fn extract_component(source: &str, file_path: &Path) -> NgcResult<Option<ExtractedComponent>> {
    let allocator = Allocator::new();
    let source_type =
        SourceType::from_path(file_path).map_err(|_| NgcError::TemplateCompileError {
            path: file_path.to_path_buf(),
            message: "unsupported file extension".to_string(),
        })?;

    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Err(NgcError::TemplateCompileError {
            path: file_path.to_path_buf(),
            message: "parser panicked".to_string(),
        });
    }

    // Find @angular/core import span and non-Component imports
    let mut angular_core_import_span = None;
    let mut other_angular_core_imports = Vec::new();

    for stmt in &parsed.program.body {
        if let Statement::ImportDeclaration(import) = stmt {
            if import.source.value.as_str() == "@angular/core" {
                angular_core_import_span = Some((import.span.start, import.span.end));
                if let Some(specifiers) = &import.specifiers {
                    for spec in specifiers {
                        if let oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) = spec {
                            let name = s.local.name.as_str();
                            if name != "Component" {
                                other_angular_core_imports.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Find class with @Component decorator
    for stmt in &parsed.program.body {
        let (class, export_start) = match stmt {
            Statement::ExportDefaultDeclaration(export) => {
                if let oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(class) =
                    &export.declaration
                {
                    (class, Some(export.span.start))
                } else {
                    continue;
                }
            }
            Statement::ExportNamedDeclaration(export) => {
                if let Some(oxc_ast::ast::Declaration::ClassDeclaration(class)) =
                    &export.declaration
                {
                    (class, Some(export.span.start))
                } else {
                    continue;
                }
            }
            Statement::ClassDeclaration(class) => (class, None),
            _ => continue,
        };

        let component_decorator = find_component_decorator(&class.decorators);
        if component_decorator.is_none() {
            continue;
        }
        let decorator = component_decorator.expect("checked above");

        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.to_string())
            .unwrap_or_else(|| "AnonymousComponent".to_string());

        let class_body_start = class.body.span.start;
        let class_keyword_start = class.span.start;

        let metadata = extract_decorator_metadata(source, decorator)?;

        return Ok(Some(ExtractedComponent {
            class_name,
            selector: metadata.selector,
            template: metadata.template,
            template_url: metadata.template_url,
            standalone: metadata.standalone,
            imports_source: metadata.imports_source,
            imports_identifiers: metadata.imports_identifiers,
            decorator_span: (decorator.span.start, decorator.span.end),
            class_body_start,
            export_keyword_start: export_start,
            class_keyword_start,
            angular_core_import_span,
            other_angular_core_imports,
            styles_source: metadata.styles_source,
        }));
    }

    Ok(None)
}

/// Find the `@Component(...)` decorator in a list of decorators.
fn find_component_decorator<'a>(decorators: &'a [Decorator<'a>]) -> Option<&'a Decorator<'a>> {
    decorators.iter().find(|d| {
        if let Expression::CallExpression(call) = &d.expression {
            if let Expression::Identifier(ident) = &call.callee {
                return ident.name.as_str() == "Component";
            }
        }
        false
    })
}

/// Metadata extracted from the decorator's argument object.
struct DecoratorMetadata {
    selector: String,
    template: Option<String>,
    template_url: Option<String>,
    standalone: bool,
    imports_source: Option<String>,
    imports_identifiers: Vec<String>,
    styles_source: Option<String>,
}

/// Extract metadata from the `@Component({...})` decorator argument.
fn extract_decorator_metadata(source: &str, decorator: &Decorator) -> NgcResult<DecoratorMetadata> {
    let call = match &decorator.expression {
        Expression::CallExpression(call) => call,
        _ => {
            return Ok(DecoratorMetadata {
                selector: String::new(),
                template: None,
                template_url: None,
                standalone: false,
                imports_source: None,
                imports_identifiers: Vec::new(),
                styles_source: None,
            });
        }
    };

    let arg = match call.arguments.first() {
        Some(Argument::ObjectExpression(obj)) => obj,
        _ => {
            return Ok(DecoratorMetadata {
                selector: String::new(),
                template: None,
                template_url: None,
                standalone: false,
                imports_source: None,
                imports_identifiers: Vec::new(),
                styles_source: None,
            });
        }
    };

    let mut selector = String::new();
    let mut template = None;
    let mut template_url = None;
    let mut standalone = false;
    let mut imports_source = None;
    let mut imports_identifiers = Vec::new();
    let mut styles_source = None;

    for prop in &arg.properties {
        if let ObjectPropertyKind::ObjectProperty(prop) = prop {
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                _ => continue,
            };

            match key_name.as_str() {
                "selector" => {
                    if let Expression::StringLiteral(s) = &prop.value {
                        selector = s.value.to_string();
                    }
                }
                "template" => {
                    if let Expression::StringLiteral(s) = &prop.value {
                        template = Some(s.value.to_string());
                    } else if let Expression::TemplateLiteral(tpl) = &prop.value {
                        // Template literal with no expressions = static string
                        if tpl.expressions.is_empty() {
                            let text: String =
                                tpl.quasis.iter().map(|q| q.value.raw.as_str()).collect();
                            template = Some(text);
                        }
                    }
                }
                "templateUrl" => {
                    if let Expression::StringLiteral(s) = &prop.value {
                        template_url = Some(s.value.to_string());
                    }
                }
                "standalone" => {
                    if let Expression::BooleanLiteral(b) = &prop.value {
                        standalone = b.value;
                    }
                }
                "styles" | "styleUrl" | "styleUrls" => {
                    let start = prop.value.span().start as usize;
                    let end = prop.value.span().end as usize;
                    if start < source.len() && end <= source.len() {
                        styles_source = Some(source[start..end].to_string());
                    }
                }
                "imports" => {
                    // Capture the raw source text for the imports array
                    let start = prop.value.span().start as usize;
                    let end = prop.value.span().end as usize;
                    if start < source.len() && end <= source.len() {
                        imports_source = Some(source[start..end].to_string());
                    }
                    // Also extract individual identifiers
                    if let Expression::ArrayExpression(arr) = &prop.value {
                        for elem in &arr.elements {
                            if let oxc_ast::ast::ArrayExpressionElement::Identifier(ident) = elem {
                                imports_identifiers.push(ident.name.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(DecoratorMetadata {
        selector,
        template,
        template_url,
        standalone,
        imports_source,
        imports_identifiers,
        styles_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_path() -> PathBuf {
        PathBuf::from("test.ts")
    }

    #[test]
    fn test_extract_simple_component() {
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-root',
  standalone: true,
  template: '<h1>Hello</h1>'
})
export class AppComponent {
  title = 'app';
}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        assert_eq!(result.class_name, "AppComponent");
        assert_eq!(result.selector, "app-root");
        assert_eq!(result.template.as_deref(), Some("<h1>Hello</h1>"));
        assert!(result.standalone);
        assert!(result.template_url.is_none());
    }

    #[test]
    fn test_extract_with_imports_array() {
        let source = r#"import { Component } from '@angular/core';
import { RouterOutlet } from '@angular/router';

@Component({
  selector: 'app-root',
  standalone: true,
  imports: [RouterOutlet],
  template: '<router-outlet />'
})
export class AppComponent {}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        assert_eq!(result.imports_identifiers, vec!["RouterOutlet"]);
        assert!(result.imports_source.is_some());
    }

    #[test]
    fn test_extract_template_url() {
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-root',
  templateUrl: './app.component.html'
})
export class AppComponent {}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        assert_eq!(result.template_url.as_deref(), Some("./app.component.html"));
        // templateUrl is now natively compiled (no JIT fallback)
        assert!(!result.needs_jit_fallback());
    }

    #[test]
    fn test_no_component_returns_none() {
        let source = "export class PlainClass { x = 1; }\n";
        let result = extract_component(source, &test_path()).expect("should not error");
        assert!(result.is_none());
    }

    #[test]
    fn test_ngif_natively_compiled() {
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-test',
  template: '<div *ngIf="show">Hello</div>'
})
export class TestComponent {}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        // *ngIf is now natively compiled (no JIT fallback)
        assert!(!result.needs_jit_fallback());
    }

    #[test]
    fn test_template_literal() {
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-root',
  template: `<h1>Hello</h1>`
})
export class AppComponent {}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        assert_eq!(result.template.as_deref(), Some("<h1>Hello</h1>"));
    }

    #[test]
    fn test_angular_core_import_tracking() {
        let source = r#"import { Component, OnInit } from '@angular/core';

@Component({
  selector: 'app-root',
  template: '<h1>Hello</h1>'
})
export class AppComponent implements OnInit {
  ngOnInit() {}
}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        assert!(result.angular_core_import_span.is_some());
        assert_eq!(result.other_angular_core_imports, vec!["OnInit"]);
    }
}
