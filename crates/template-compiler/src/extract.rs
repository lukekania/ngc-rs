use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, Class, Decorator, Expression, FormalParameter, FormalParameters,
    MethodDefinitionKind, ObjectPropertyKind, PropertyKey, Statement, TSTypeAnnotation,
};
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
    find_decorator_by_name(decorators, "Component")
}

/// Find a decorator by name in a list of decorators.
fn find_decorator_by_name<'a>(
    decorators: &'a [Decorator<'a>],
    name: &str,
) -> Option<&'a Decorator<'a>> {
    decorators.iter().find(|d| {
        if let Expression::CallExpression(call) = &d.expression {
            if let Expression::Identifier(ident) = &call.callee {
                return ident.name.as_str() == name;
            }
        }
        false
    })
}

/// A constructor parameter with dependency injection metadata.
#[derive(Debug, Clone)]
pub struct ConstructorParam {
    /// The TypeScript type annotation name (injection token).
    pub type_name: Option<String>,
    /// Explicit `@Inject(TOKEN)` token override.
    pub inject_token: Option<String>,
    /// Whether `@Optional()` is present.
    pub optional: bool,
    /// Whether `@Self()` is present.
    pub self_: bool,
    /// Whether `@SkipSelf()` is present.
    pub skip_self: bool,
    /// Whether `@Host()` is present.
    pub host: bool,
}

/// Common fields shared by all Angular decorator extractions for rewriting.
#[derive(Debug, Clone)]
pub struct DecoratorCommon {
    /// Byte offset range of the full decorator (from `@` to closing `)`).
    pub decorator_span: (u32, u32),
    /// Byte offset of the class body opening `{`.
    pub class_body_start: u32,
    /// The source text span of the `@angular/core` import declaration.
    pub angular_core_import_span: Option<(u32, u32)>,
    /// Named imports from `@angular/core` other than the decorator itself.
    pub other_angular_core_imports: Vec<String>,
}

/// Metadata extracted from an `@Injectable` decorator.
#[derive(Debug, Clone)]
pub struct ExtractedInjectable {
    /// The class name (e.g. `AuthService`).
    pub class_name: String,
    /// The `providedIn` value as raw source text (e.g. `'root'`).
    pub provided_in: Option<String>,
    /// The `useFactory` expression as raw source text.
    pub use_factory: Option<String>,
    /// The `useClass` expression as raw source text.
    pub use_class: Option<String>,
    /// The `useValue` expression as raw source text.
    pub use_value: Option<String>,
    /// The `useExisting` expression as raw source text.
    pub use_existing: Option<String>,
    /// Constructor parameters for DI-aware factory generation.
    pub constructor_params: Vec<ConstructorParam>,
    /// Common decorator fields for rewriting.
    pub common: DecoratorCommon,
}

/// Metadata extracted from a `@Directive` decorator.
#[derive(Debug, Clone)]
pub struct ExtractedDirective {
    /// The class name (e.g. `HighlightDirective`).
    pub class_name: String,
    /// The directive selector (e.g. `[appHighlight]`).
    pub selector: Option<String>,
    /// Whether the directive is standalone.
    pub standalone: bool,
    /// Raw source text of the `inputs` object/array.
    pub inputs_source: Option<String>,
    /// Raw source text of the `outputs` object/array.
    pub outputs_source: Option<String>,
    /// The `exportAs` value.
    pub export_as: Option<String>,
    /// Constructor parameters for DI-aware factory generation.
    pub constructor_params: Vec<ConstructorParam>,
    /// Common decorator fields for rewriting.
    pub common: DecoratorCommon,
}

/// Metadata extracted from a `@Pipe` decorator.
#[derive(Debug, Clone)]
pub struct ExtractedPipe {
    /// The class name (e.g. `DateFormatPipe`).
    pub class_name: String,
    /// The pipe name (e.g. `dateFormat`).
    pub pipe_name: String,
    /// The `pure` flag value.
    pub pure: Option<bool>,
    /// Whether the pipe is standalone.
    pub standalone: bool,
    /// Constructor parameters for DI-aware factory generation.
    pub constructor_params: Vec<ConstructorParam>,
    /// Common decorator fields for rewriting.
    pub common: DecoratorCommon,
}

/// Metadata extracted from an `@NgModule` decorator.
#[derive(Debug, Clone)]
pub struct ExtractedNgModule {
    /// The class name (e.g. `AppModule`).
    pub class_name: String,
    /// Raw source text of the `declarations` array.
    pub declarations_source: Option<String>,
    /// Raw source text of the `imports` array.
    pub imports_source: Option<String>,
    /// Raw source text of the `exports` array.
    pub exports_source: Option<String>,
    /// Raw source text of the `providers` array.
    pub providers_source: Option<String>,
    /// Raw source text of the `bootstrap` array.
    pub bootstrap_source: Option<String>,
    /// Common decorator fields for rewriting.
    pub common: DecoratorCommon,
}

/// Extract `@Injectable` metadata from a TypeScript source file.
///
/// Returns `None` if no `@Injectable` decorator is found.
pub fn extract_injectable(
    source: &str,
    file_path: &Path,
) -> NgcResult<Option<ExtractedInjectable>> {
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

    let (angular_core_import_span, other_imports) =
        find_angular_core_imports(&parsed.program.body, "Injectable");

    for stmt in &parsed.program.body {
        let (class, export_start) = match_class_statement(stmt);
        let class = match class {
            Some(c) => c,
            None => continue,
        };
        let _ = export_start;

        let decorator = match find_decorator_by_name(&class.decorators, "Injectable") {
            Some(d) => d,
            None => continue,
        };

        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.to_string())
            .unwrap_or_else(|| "Anonymous".to_string());

        let class_body_start = class.body.span.start;
        let constructor_params = extract_constructor_params(source, &class.body);

        // Extract metadata from decorator arguments
        let mut provided_in = None;
        let mut use_factory = None;
        let mut use_class = None;
        let mut use_value = None;
        let mut use_existing = None;

        if let Expression::CallExpression(call) = &decorator.expression {
            if let Some(Argument::ObjectExpression(obj)) = call.arguments.first() {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(prop) = prop {
                        let key_name = match &prop.key {
                            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                            _ => continue,
                        };
                        let val_src = source_text_of(source, &prop.value);
                        match key_name.as_str() {
                            "providedIn" => provided_in = val_src,
                            "useFactory" => use_factory = val_src,
                            "useClass" => use_class = val_src,
                            "useValue" => use_value = val_src,
                            "useExisting" => use_existing = val_src,
                            _ => {}
                        }
                    }
                }
            }
        }

        return Ok(Some(ExtractedInjectable {
            class_name,
            provided_in,
            use_factory,
            use_class,
            use_value,
            use_existing,
            constructor_params,
            common: DecoratorCommon {
                decorator_span: (decorator.span.start, decorator.span.end),
                class_body_start,
                angular_core_import_span,
                other_angular_core_imports: other_imports,
            },
        }));
    }

    Ok(None)
}

/// Extract `@Directive` metadata from a TypeScript source file.
///
/// Returns `None` if no `@Directive` decorator is found.
pub fn extract_directive(
    source: &str,
    file_path: &Path,
) -> NgcResult<Option<ExtractedDirective>> {
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

    let (angular_core_import_span, other_imports) =
        find_angular_core_imports(&parsed.program.body, "Directive");

    for stmt in &parsed.program.body {
        let (class, _) = match_class_statement(stmt);
        let class = match class {
            Some(c) => c,
            None => continue,
        };

        let decorator = match find_decorator_by_name(&class.decorators, "Directive") {
            Some(d) => d,
            None => continue,
        };

        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.to_string())
            .unwrap_or_else(|| "Anonymous".to_string());

        let class_body_start = class.body.span.start;
        let constructor_params = extract_constructor_params(source, &class.body);

        let mut selector = None;
        let mut standalone = false;
        let mut inputs_source = None;
        let mut outputs_source = None;
        let mut export_as = None;

        if let Expression::CallExpression(call) = &decorator.expression {
            if let Some(Argument::ObjectExpression(obj)) = call.arguments.first() {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(prop) = prop {
                        let key_name = match &prop.key {
                            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                            _ => continue,
                        };
                        match key_name.as_str() {
                            "selector" => {
                                if let Expression::StringLiteral(s) = &prop.value {
                                    selector = Some(s.value.to_string());
                                }
                            }
                            "standalone" => {
                                if let Expression::BooleanLiteral(b) = &prop.value {
                                    standalone = b.value;
                                }
                            }
                            "inputs" => inputs_source = source_text_of(source, &prop.value),
                            "outputs" => outputs_source = source_text_of(source, &prop.value),
                            "exportAs" => {
                                if let Expression::StringLiteral(s) = &prop.value {
                                    export_as = Some(s.value.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        return Ok(Some(ExtractedDirective {
            class_name,
            selector,
            standalone,
            inputs_source,
            outputs_source,
            export_as,
            constructor_params,
            common: DecoratorCommon {
                decorator_span: (decorator.span.start, decorator.span.end),
                class_body_start,
                angular_core_import_span,
                other_angular_core_imports: other_imports,
            },
        }));
    }

    Ok(None)
}

/// Extract `@Pipe` metadata from a TypeScript source file.
///
/// Returns `None` if no `@Pipe` decorator is found.
pub fn extract_pipe(source: &str, file_path: &Path) -> NgcResult<Option<ExtractedPipe>> {
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

    let (angular_core_import_span, other_imports) =
        find_angular_core_imports(&parsed.program.body, "Pipe");

    for stmt in &parsed.program.body {
        let (class, _) = match_class_statement(stmt);
        let class = match class {
            Some(c) => c,
            None => continue,
        };

        let decorator = match find_decorator_by_name(&class.decorators, "Pipe") {
            Some(d) => d,
            None => continue,
        };

        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.to_string())
            .unwrap_or_else(|| "Anonymous".to_string());

        let class_body_start = class.body.span.start;
        let constructor_params = extract_constructor_params(source, &class.body);

        let mut pipe_name = String::new();
        let mut pure = None;
        let mut standalone = false;

        if let Expression::CallExpression(call) = &decorator.expression {
            if let Some(Argument::ObjectExpression(obj)) = call.arguments.first() {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(prop) = prop {
                        let key_name = match &prop.key {
                            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                            _ => continue,
                        };
                        match key_name.as_str() {
                            "name" => {
                                if let Expression::StringLiteral(s) = &prop.value {
                                    pipe_name = s.value.to_string();
                                }
                            }
                            "pure" => {
                                if let Expression::BooleanLiteral(b) = &prop.value {
                                    pure = Some(b.value);
                                }
                            }
                            "standalone" => {
                                if let Expression::BooleanLiteral(b) = &prop.value {
                                    standalone = b.value;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        return Ok(Some(ExtractedPipe {
            class_name,
            pipe_name,
            pure,
            standalone,
            constructor_params,
            common: DecoratorCommon {
                decorator_span: (decorator.span.start, decorator.span.end),
                class_body_start,
                angular_core_import_span,
                other_angular_core_imports: other_imports,
            },
        }));
    }

    Ok(None)
}

/// Extract `@NgModule` metadata from a TypeScript source file.
///
/// Returns `None` if no `@NgModule` decorator is found.
pub fn extract_ng_module(
    source: &str,
    file_path: &Path,
) -> NgcResult<Option<ExtractedNgModule>> {
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

    let (angular_core_import_span, other_imports) =
        find_angular_core_imports(&parsed.program.body, "NgModule");

    for stmt in &parsed.program.body {
        let (class, _) = match_class_statement(stmt);
        let class = match class {
            Some(c) => c,
            None => continue,
        };

        let decorator = match find_decorator_by_name(&class.decorators, "NgModule") {
            Some(d) => d,
            None => continue,
        };

        let class_name = class
            .id
            .as_ref()
            .map(|id| id.name.to_string())
            .unwrap_or_else(|| "Anonymous".to_string());

        let class_body_start = class.body.span.start;

        let mut declarations_source = None;
        let mut imports_source = None;
        let mut exports_source = None;
        let mut providers_source = None;
        let mut bootstrap_source = None;

        if let Expression::CallExpression(call) = &decorator.expression {
            if let Some(Argument::ObjectExpression(obj)) = call.arguments.first() {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(prop) = prop {
                        let key_name = match &prop.key {
                            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                            _ => continue,
                        };
                        let val_src = source_text_of(source, &prop.value);
                        match key_name.as_str() {
                            "declarations" => declarations_source = val_src,
                            "imports" => imports_source = val_src,
                            "exports" => exports_source = val_src,
                            "providers" => providers_source = val_src,
                            "bootstrap" => bootstrap_source = val_src,
                            _ => {}
                        }
                    }
                }
            }
        }

        return Ok(Some(ExtractedNgModule {
            class_name,
            declarations_source,
            imports_source,
            exports_source,
            providers_source,
            bootstrap_source,
            common: DecoratorCommon {
                decorator_span: (decorator.span.start, decorator.span.end),
                class_body_start,
                angular_core_import_span,
                other_angular_core_imports: other_imports,
            },
        }));
    }

    Ok(None)
}

/// Extract a class declaration from a statement, handling export wrappers.
fn match_class_statement<'a>(stmt: &'a Statement<'a>) -> (Option<&'a Class<'a>>, Option<u32>) {
    match stmt {
        Statement::ExportDefaultDeclaration(export) => {
            if let oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(class) =
                &export.declaration
            {
                (Some(class), Some(export.span.start))
            } else {
                (None, None)
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(oxc_ast::ast::Declaration::ClassDeclaration(class)) = &export.declaration {
                (Some(class), Some(export.span.start))
            } else {
                (None, None)
            }
        }
        Statement::ClassDeclaration(class) => (Some(class), None),
        _ => (None, None),
    }
}

/// Find `@angular/core` import span and non-decorator imports.
fn find_angular_core_imports(
    body: &[Statement<'_>],
    decorator_name: &str,
) -> (Option<(u32, u32)>, Vec<String>) {
    let mut span = None;
    let mut others = Vec::new();

    for stmt in body {
        if let Statement::ImportDeclaration(import) = stmt {
            if import.source.value.as_str() == "@angular/core" {
                span = Some((import.span.start, import.span.end));
                if let Some(specifiers) = &import.specifiers {
                    for spec in specifiers {
                        if let oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) = spec {
                            let name = s.local.name.as_str();
                            if name != decorator_name {
                                others.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    (span, others)
}

/// Get the raw source text of an expression.
fn source_text_of(source: &str, expr: &Expression<'_>) -> Option<String> {
    let start = expr.span().start as usize;
    let end = expr.span().end as usize;
    if start < source.len() && end <= source.len() {
        Some(source[start..end].to_string())
    } else {
        None
    }
}

/// Extract constructor parameters from a class body for DI-aware factory generation.
fn extract_constructor_params(
    source: &str,
    class_body: &oxc_ast::ast::ClassBody<'_>,
) -> Vec<ConstructorParam> {
    for element in &class_body.body {
        if let oxc_ast::ast::ClassElement::MethodDefinition(method) = element {
            if method.kind == MethodDefinitionKind::Constructor {
                return extract_params_from_formal(source, &method.value.params);
            }
        }
    }
    Vec::new()
}

/// Extract DI metadata from formal parameters.
fn extract_params_from_formal(
    source: &str,
    params: &FormalParameters<'_>,
) -> Vec<ConstructorParam> {
    params
        .items
        .iter()
        .map(|param| extract_single_param(source, param))
        .collect()
}

/// Extract DI metadata from a single formal parameter.
fn extract_single_param(source: &str, param: &FormalParameter<'_>) -> ConstructorParam {
    let type_name = param
        .type_annotation
        .as_ref()
        .and_then(|ann| extract_type_name(source, ann));

    let mut inject_token = None;
    let mut optional = false;
    let mut self_ = false;
    let mut skip_self = false;
    let mut host = false;

    for decorator in &param.decorators {
        if let Expression::CallExpression(call) = &decorator.expression {
            if let Expression::Identifier(ident) = &call.callee {
                match ident.name.as_str() {
                    "Inject" => {
                        if let Some(arg) = call.arguments.first() {
                            let start = arg.span().start as usize;
                            let end = arg.span().end as usize;
                            if start < source.len() && end <= source.len() {
                                inject_token = Some(source[start..end].to_string());
                            }
                        }
                    }
                    "Optional" => optional = true,
                    "Self" => self_ = true,
                    "SkipSelf" => skip_self = true,
                    "Host" => host = true,
                    _ => {}
                }
            }
        }
    }

    ConstructorParam {
        type_name,
        inject_token,
        optional,
        self_,
        skip_self,
        host,
    }
}

/// Extract the type name from a TypeScript type annotation.
fn extract_type_name(source: &str, annotation: &TSTypeAnnotation<'_>) -> Option<String> {
    let start = annotation.type_annotation.span().start as usize;
    let end = annotation.type_annotation.span().end as usize;
    if start < source.len() && end <= source.len() {
        Some(source[start..end].to_string())
    } else {
        None
    }
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

    #[test]
    fn test_extract_injectable_basic() {
        let source = r#"import { Injectable } from '@angular/core';

@Injectable({ providedIn: 'root' })
export class AuthService {
}
"#;
        let result = extract_injectable(source, &test_path())
            .expect("should extract")
            .expect("should find injectable");
        assert_eq!(result.class_name, "AuthService");
        assert_eq!(result.provided_in.as_deref(), Some("'root'"));
        assert!(result.constructor_params.is_empty());
    }

    #[test]
    fn test_extract_injectable_with_deps() {
        let source = r#"import { Injectable } from '@angular/core';
import { HttpClient } from '@angular/common/http';

@Injectable({ providedIn: 'root' })
export class DataService {
  constructor(private http: HttpClient) {}
}
"#;
        let result = extract_injectable(source, &test_path())
            .expect("should extract")
            .expect("should find injectable");
        assert_eq!(result.class_name, "DataService");
        assert_eq!(result.constructor_params.len(), 1);
        assert_eq!(
            result.constructor_params[0].type_name.as_deref(),
            Some("HttpClient")
        );
    }

    #[test]
    fn test_extract_injectable_with_inject_decorator() {
        let source = r#"import { Injectable, Inject, Optional } from '@angular/core';

@Injectable()
export class MyService {
  constructor(@Inject('API_URL') private url: string, @Optional() private dep: SomeDep) {}
}
"#;
        let result = extract_injectable(source, &test_path())
            .expect("should extract")
            .expect("should find injectable");
        assert_eq!(result.constructor_params.len(), 2);
        assert_eq!(
            result.constructor_params[0].inject_token.as_deref(),
            Some("'API_URL'")
        );
        assert!(result.constructor_params[1].optional);
    }

    #[test]
    fn test_extract_directive() {
        let source = r#"import { Directive } from '@angular/core';

@Directive({
  selector: '[appHighlight]',
  standalone: true
})
export class HighlightDirective {
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.class_name, "HighlightDirective");
        assert_eq!(result.selector.as_deref(), Some("[appHighlight]"));
        assert!(result.standalone);
    }

    #[test]
    fn test_extract_pipe() {
        let source = r#"import { Pipe, PipeTransform } from '@angular/core';

@Pipe({
  name: 'dateFormat',
  standalone: true,
  pure: false
})
export class DateFormatPipe implements PipeTransform {
  transform(value: any): string { return ''; }
}
"#;
        let result = extract_pipe(source, &test_path())
            .expect("should extract")
            .expect("should find pipe");
        assert_eq!(result.class_name, "DateFormatPipe");
        assert_eq!(result.pipe_name, "dateFormat");
        assert_eq!(result.pure, Some(false));
        assert!(result.standalone);
        // PipeTransform should be in other imports
        assert!(result
            .common
            .other_angular_core_imports
            .contains(&"PipeTransform".to_string()));
    }

    #[test]
    fn test_extract_ng_module() {
        let source = r#"import { NgModule } from '@angular/core';
import { CommonModule } from '@angular/common';
import { AppComponent } from './app.component';

@NgModule({
  declarations: [AppComponent],
  imports: [CommonModule],
  exports: [AppComponent],
  bootstrap: [AppComponent]
})
export class AppModule {}
"#;
        let result = extract_ng_module(source, &test_path())
            .expect("should extract")
            .expect("should find ng module");
        assert_eq!(result.class_name, "AppModule");
        assert!(result.declarations_source.is_some());
        assert!(result.imports_source.is_some());
        assert!(result.exports_source.is_some());
        assert!(result.bootstrap_source.is_some());
    }

    #[test]
    fn test_no_injectable_returns_none() {
        let source = "export class PlainClass { x = 1; }\n";
        let result = extract_injectable(source, &test_path()).expect("should not error");
        assert!(result.is_none());
    }
}
