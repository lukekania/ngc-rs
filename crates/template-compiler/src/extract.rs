use std::path::Path;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, Class, ClassElement, Decorator, Expression, FormalParameter,
    FormalParameters, MethodDefinitionKind, ObjectPropertyKind, PropertyKey, Statement,
    TSTypeAnnotation,
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
    ///
    /// Retained for codegen fast-path when no preprocessing is needed. When
    /// `style_urls` is non-empty or `inlineStyleLanguage` is non-CSS, the
    /// post-extraction step in `compile_component` rewrites this field to a
    /// synthesized `[`compiled-css`]` array of preprocessed CSS.
    pub styles_source: Option<String>,
    /// Inline CSS contents from `styles: [...]` — one entry per element
    /// (template-literal body or string literal value, with no surrounding
    /// backticks/quotes).
    pub inline_styles: Vec<String>,
    /// Relative paths from `styleUrl: '...'` or `styleUrls: [...]`. These
    /// are resolved against the component's file directory and read during
    /// `compile_component` so preprocessors can run on their contents.
    pub style_urls: Vec<String>,
    /// Property names decorated with `@Input()`.
    pub input_properties: Vec<String>,
    /// Method-level `@HostListener` extractions (e.g. `('click', ['$event'])`).
    pub host_listeners: Vec<HostListenerSpec>,
    /// Field/property-level `@HostBinding` extractions (e.g. `('class.active')`).
    pub host_bindings: Vec<HostBindingSpec>,
    /// Raw source text of the `animations` array (e.g. `[trigger('fade', [...])]`).
    /// Passed through to the `defineComponent({ data: { animation: [...] } })`
    /// field so Angular's runtime can register triggers with the animation renderer.
    pub animations_source: Option<String>,
    /// Raw source text of the `hostDirectives` array (Angular 15+ composition).
    /// Passed through to a `ɵɵHostDirectivesFeature(...)` call in the
    /// `features` array. Accepts both bare class refs (e.g. `[Foo]`) and the
    /// object form (`[{directive: Foo, inputs: ['x'], outputs: [...]}]`).
    pub host_directives_source: Option<String>,
    /// Class fields initialised with `input(...)` / `input.required(...)`.
    pub signal_inputs: Vec<SignalInputSpec>,
    /// Class fields initialised with `output(...)`.
    pub signal_outputs: Vec<SignalOutputSpec>,
    /// Class fields initialised with `model(...)` / `model.required(...)`.
    pub signal_models: Vec<SignalModelSpec>,
    /// Class fields initialised with `viewChild`/`viewChildren`/`contentChild`/`contentChildren`.
    pub signal_queries: Vec<SignalQuerySpec>,
    /// Numeric `ChangeDetectionStrategy` value from the decorator's
    /// `changeDetection` property (`0` for `OnPush`, `1` for `Default`),
    /// or `None` when the property is absent. Threaded through to
    /// `defineComponent({ changeDetection })` — required for zoneless
    /// apps so `OnPush` components actually mark themselves dirty when
    /// signal writes happen inside event handlers.
    pub change_detection: Option<u32>,
}

/// A method-level `@HostListener(event, [args])` extraction.
#[derive(Debug, Clone)]
pub struct HostListenerSpec {
    /// Event name passed to the decorator (e.g. `"click"`, `"window:resize"`).
    pub event: String,
    /// The JS expression invoked by `ɵɵlistener` — already shaped as
    /// `methodName(...args)`. Component members get `ctx.` prefixed downstream
    /// by `host_codegen::compile_host_expression`.
    pub handler_expression: String,
}

/// A field/property-level `@HostBinding(target?)` extraction.
#[derive(Debug, Clone)]
pub struct HostBindingSpec {
    /// Decorator argument: bare property name, `attr.X`, `class.Y`, `style.Z`,
    /// or `style.Z.unit`. Falls back to the property name when the decorator
    /// is called with no arguments.
    pub target: String,
    /// The class field/getter that supplies the value. Compiled to `ctx.<name>`
    /// at codegen time.
    pub property_name: String,
}

/// A class field initialised with `input(...)` or `input.required(...)`.
///
/// Captures everything `ɵɵdefineDirective` / `ɵɵdefineComponent` need to
/// emit a runtime `inputs` entry with the `SignalBased` flag set: the
/// optional alias (becomes `publicName`), whether it's required, and the
/// raw source of the `transform` reference so it survives codegen as a
/// callable expression rather than a stringified value.
#[derive(Debug, Clone)]
pub struct SignalInputSpec {
    /// The class field name (e.g. `name` for `name = input(...)`).
    pub property_name: String,
    /// `alias` from the options object, if any. Defaults to `property_name`.
    pub alias: Option<String>,
    /// Whether the call site was `input.required(...)` (vs. `input(...)`).
    pub is_required: bool,
    /// Raw source text of the `transform` reference, if present (e.g.
    /// `trimString` or `(v) => v.trim()`). `None` means no transform.
    /// For signal-based inputs the runtime reads the transform off the
    /// signal field at write time, so codegen doesn't actually splice
    /// this into the inputs def — but we still capture it so future
    /// passes (lints, source maps, type checking) have it available.
    #[allow(dead_code)]
    pub transform_source: Option<String>,
}

/// A class field initialised with `output(...)`.
///
/// Outputs only need a public name — the runtime treats them as plain
/// strings in the `outputs` map. An alias makes `publicName` differ from
/// `property_name`.
#[derive(Debug, Clone)]
pub struct SignalOutputSpec {
    /// The class field name.
    pub property_name: String,
    /// `alias` from the options object, if any.
    pub alias: Option<String>,
}

/// A class field initialised with `model(...)` or `model.required(...)`.
///
/// `model<T>()` desugars to a paired signal input + `<name>Change` output,
/// so every model produces one entry in each runtime map. The alias (if
/// any) is the public name of the input; the output is always
/// `<alias>Change`.
#[derive(Debug, Clone)]
pub struct SignalModelSpec {
    pub property_name: String,
    pub alias: Option<String>,
    pub is_required: bool,
}

/// A class field initialised with one of the four signal-query factories
/// (`viewChild`, `viewChildren`, `contentChild`, `contentChildren`).
///
/// `kind` carries both the create-call dispatch (view vs. content) and
/// whether the query is single-shot (`first: true`) or multi (`false`).
/// `predicate_source` is preserved verbatim — the predicate may be a
/// class reference, an `InjectionToken`, or a string literal, all of which
/// the runtime accepts unchanged.
#[derive(Debug, Clone)]
pub struct SignalQuerySpec {
    pub property_name: String,
    pub kind: SignalQueryKind,
    /// Raw source text of the predicate argument
    /// (e.g. `MyChildComponent`, `'ref'`, `MY_TOKEN`).
    pub predicate_source: String,
    /// Raw source text of `read:` from the options object, if any.
    pub read_source: Option<String>,
    /// `static: true` from the options object — defaults to `false`.
    pub is_static: bool,
    /// User-supplied `descendants` option, or `None` to use the
    /// kind-specific default. Tracking this separately from the kind
    /// lets `descendants: false` on a `viewChild` (which defaults to
    /// `true`) survive into codegen instead of getting masked by the
    /// default.
    pub descendants: Option<bool>,
}

/// Which signal-query factory created the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalQueryKind {
    /// `viewChild()` — single, descendants, dynamic.
    ViewChild,
    /// `viewChildren()` — multi, descendants, dynamic.
    ViewChildren,
    /// `contentChild()` — single, defaults to non-descendants.
    ContentChild,
    /// `contentChildren()` — multi, defaults to non-descendants.
    ContentChildren,
}

impl SignalQueryKind {
    /// `true` for `viewChild` / `contentChild` (single) variants.
    /// Currently unused in codegen (the always-on
    /// `emitDistinctChangesOnly` flag for signal queries means we don't
    /// branch on first/multi for signal queries), but kept for parity
    /// with linker-side query handling and as a future-proofing hook.
    #[allow(dead_code)]
    pub fn is_first(self) -> bool {
        matches!(self, Self::ViewChild | Self::ContentChild)
    }

    /// `true` for `viewChild` / `viewChildren` (i.e. emitted into
    /// `viewQuery`, not `contentQueries`).
    pub fn is_view(self) -> bool {
        matches!(self, Self::ViewChild | Self::ViewChildren)
    }

    /// Default `descendants` flag for the kind, matching Angular's
    /// signal-query compiler defaults. **Only `contentChildren` defaults
    /// to `false`** — `viewChild` / `viewChildren` / `contentChild`
    /// all default to `true`. (Note: this differs from decorator-style
    /// `@ContentChild`, which defaults to `descendants: false`.)
    pub fn default_descendants(self) -> bool {
        !matches!(self, Self::ContentChildren)
    }
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
        let error_msgs: Vec<String> = parsed.errors.iter().map(|e| format!("{e}")).collect();
        let detail = if error_msgs.is_empty() {
            "parser panicked (no details)".to_string()
        } else {
            format!("parser panicked: {}", error_msgs.join("; "))
        };
        return Err(NgcError::TemplateCompileError {
            path: file_path.to_path_buf(),
            message: detail,
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

        // Extract @Input() decorated properties from class body
        let mut input_properties = Vec::new();
        for member in &class.body.body {
            if let oxc_ast::ast::ClassElement::PropertyDefinition(prop) = member {
                if prop
                    .decorators
                    .iter()
                    .any(|d| find_decorator_by_name(std::slice::from_ref(d), "Input").is_some())
                {
                    if let oxc_ast::ast::PropertyKey::StaticIdentifier(id) = &prop.key {
                        input_properties.push(id.name.to_string());
                    }
                }
            }
        }

        let host_listeners = extract_host_listeners(&class.body);
        let host_bindings = extract_host_bindings(&class.body);
        let SignalMembers {
            inputs: signal_inputs,
            outputs: signal_outputs,
            models: signal_models,
            queries: signal_queries,
        } = extract_signal_members(source, &class.body);

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
            inline_styles: metadata.inline_styles,
            style_urls: metadata.style_urls,
            input_properties,
            host_listeners,
            host_bindings,
            animations_source: metadata.animations_source,
            host_directives_source: metadata.host_directives_source,
            signal_inputs,
            signal_outputs,
            signal_models,
            signal_queries,
            change_detection: metadata.change_detection,
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
    /// Method-level `@HostListener` extractions.
    pub host_listeners: Vec<HostListenerSpec>,
    /// Field/property-level `@HostBinding` extractions.
    pub host_bindings: Vec<HostBindingSpec>,
    /// Raw source text of the `hostDirectives` array (Angular 15+ composition).
    pub host_directives_source: Option<String>,
    /// Class fields initialised with `input(...)` / `input.required(...)`.
    pub signal_inputs: Vec<SignalInputSpec>,
    /// Class fields initialised with `output(...)`.
    pub signal_outputs: Vec<SignalOutputSpec>,
    /// Class fields initialised with `model(...)` / `model.required(...)`.
    pub signal_models: Vec<SignalModelSpec>,
    /// Class fields initialised with `viewChild`/`viewChildren`/`contentChild`/`contentChildren`.
    pub signal_queries: Vec<SignalQuerySpec>,
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
pub fn extract_directive(source: &str, file_path: &Path) -> NgcResult<Option<ExtractedDirective>> {
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
        let mut host_directives_source = None;

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
                            "hostDirectives" => {
                                host_directives_source = source_text_of(source, &prop.value);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let host_listeners = extract_host_listeners(&class.body);
        let host_bindings = extract_host_bindings(&class.body);
        let SignalMembers {
            inputs: signal_inputs,
            outputs: signal_outputs,
            models: signal_models,
            queries: signal_queries,
        } = extract_signal_members(source, &class.body);

        return Ok(Some(ExtractedDirective {
            class_name,
            selector,
            standalone,
            inputs_source,
            outputs_source,
            export_as,
            constructor_params,
            host_listeners,
            host_bindings,
            host_directives_source,
            signal_inputs,
            signal_outputs,
            signal_models,
            signal_queries,
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
pub fn extract_ng_module(source: &str, file_path: &Path) -> NgcResult<Option<ExtractedNgModule>> {
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

/// Walk a class body and collect every method-level `@HostListener(event[, args])`.
///
/// `@HostListener('click', ['$event'])` on `onClick($event)` produces a spec
/// with `event = "click"` and `handler_expression = "onClick($event)"`. The
/// handler expression is fed to `host_codegen::compile_host_expression` at
/// codegen time, which prefixes `onClick` with `ctx.`.
fn extract_host_listeners(class_body: &oxc_ast::ast::ClassBody<'_>) -> Vec<HostListenerSpec> {
    let mut listeners = Vec::new();
    for element in &class_body.body {
        let ClassElement::MethodDefinition(method) = element else {
            continue;
        };
        if method.kind == MethodDefinitionKind::Constructor {
            continue;
        }
        let method_name = match &method.key {
            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
            PropertyKey::StringLiteral(s) => s.value.to_string(),
            _ => continue,
        };
        for decorator in &method.decorators {
            let Some(call) = decorator_call(decorator, "HostListener") else {
                continue;
            };
            let event = match call.arguments.first() {
                Some(Argument::StringLiteral(s)) => s.value.to_string(),
                _ => continue,
            };
            let args_text = match call.arguments.get(1) {
                Some(Argument::ArrayExpression(arr)) => arr
                    .elements
                    .iter()
                    .filter_map(|el| match el {
                        ArrayExpressionElement::StringLiteral(s) => Some(s.value.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => String::new(),
            };
            let handler_expression = format!("{method_name}({args_text})");
            listeners.push(HostListenerSpec {
                event,
                handler_expression,
            });
        }
    }
    listeners
}

/// Walk a class body and collect every field-level `@HostBinding(target?)`.
///
/// `@HostBinding('class.active') isActive = true;` produces a spec with
/// `target = "class.active"` and `property_name = "isActive"`. When the
/// decorator argument is omitted, Angular defaults to the property name as
/// the target (matching `@HostBinding() class = '...'`).
fn extract_host_bindings(class_body: &oxc_ast::ast::ClassBody<'_>) -> Vec<HostBindingSpec> {
    let mut bindings = Vec::new();
    for element in &class_body.body {
        let (decorators, key) = match element {
            ClassElement::PropertyDefinition(prop) => (&prop.decorators, &prop.key),
            ClassElement::MethodDefinition(method)
                if method.kind == MethodDefinitionKind::Get
                    || method.kind == MethodDefinitionKind::Set =>
            {
                (&method.decorators, &method.key)
            }
            _ => continue,
        };
        let property_name = match key {
            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
            PropertyKey::StringLiteral(s) => s.value.to_string(),
            _ => continue,
        };
        for decorator in decorators {
            let Some(call) = decorator_call(decorator, "HostBinding") else {
                continue;
            };
            let target = match call.arguments.first() {
                Some(Argument::StringLiteral(s)) => s.value.to_string(),
                None => property_name.clone(),
                _ => continue,
            };
            bindings.push(HostBindingSpec {
                target,
                property_name: property_name.clone(),
            });
        }
    }
    bindings
}

/// If `decorator` is `@Name(...)`, return the call expression — otherwise `None`.
fn decorator_call<'a>(
    decorator: &'a Decorator<'a>,
    name: &str,
) -> Option<&'a oxc_ast::ast::CallExpression<'a>> {
    let Expression::CallExpression(call) = &decorator.expression else {
        return None;
    };
    let Expression::Identifier(ident) = &call.callee else {
        return None;
    };
    if ident.name.as_str() == name {
        Some(call)
    } else {
        None
    }
}

/// Bag of signal-API field initialisations found on a class body.
///
/// Returned together because all four kinds (input, output, model, query)
/// are detected in the same single pass over class fields.
#[derive(Debug, Default)]
pub(crate) struct SignalMembers {
    pub inputs: Vec<SignalInputSpec>,
    pub outputs: Vec<SignalOutputSpec>,
    pub models: Vec<SignalModelSpec>,
    pub queries: Vec<SignalQuerySpec>,
}

/// Walk a class body and pull out every signal-API field initialiser:
///   `foo = input(...)` / `input.required(...)`
///   `foo = output(...)`
///   `foo = model(...)` / `model.required(...)`
///   `foo = viewChild|viewChildren|contentChild|contentChildren(...)`
///
/// The detection is purely call-shape based: callee identifier (or
/// `<root>.required` member call) plus the field's right-hand side. We
/// don't try to resolve imports or types — anything named `input` that's
/// not the Angular factory will produce a benign signal-input emission
/// at codegen time, but TypeScript already prevents that case at the
/// authoring level.
pub(crate) fn extract_signal_members(
    source: &str,
    class_body: &oxc_ast::ast::ClassBody<'_>,
) -> SignalMembers {
    let mut out = SignalMembers::default();

    for element in &class_body.body {
        let ClassElement::PropertyDefinition(prop) = element else {
            continue;
        };
        let property_name = match &prop.key {
            PropertyKey::StaticIdentifier(id) => id.name.to_string(),
            PropertyKey::StringLiteral(s) => s.value.to_string(),
            _ => continue,
        };
        let Some(init) = &prop.value else { continue };
        let Expression::CallExpression(call) = init else {
            continue;
        };
        let Some(factory) = classify_signal_callee(&call.callee) else {
            continue;
        };

        match factory {
            SignalFactory::Input { required } => {
                let opts_idx = if required { 0 } else { 1 };
                let opts = parse_signal_options(source, call, opts_idx);
                out.inputs.push(SignalInputSpec {
                    property_name,
                    alias: opts.alias,
                    is_required: required,
                    transform_source: opts.transform,
                });
            }
            SignalFactory::Output => {
                // `output()` / `output<T>()` only accepts an optional
                // options object as its first arg (no positional default).
                let opts = parse_signal_options(source, call, 0);
                out.outputs.push(SignalOutputSpec {
                    property_name,
                    alias: opts.alias,
                });
            }
            SignalFactory::Model { required } => {
                let opts_idx = if required { 0 } else { 1 };
                let opts = parse_signal_options(source, call, opts_idx);
                out.models.push(SignalModelSpec {
                    property_name,
                    alias: opts.alias,
                    is_required: required,
                });
            }
            SignalFactory::Query { kind, .. } => {
                // Predicate handling matches Angular's compiler:
                //   * String literal `viewChild('ref')` → `['ref']`. Bare
                //     strings would otherwise be read as a `ProviderToken`
                //     and the runtime would never resolve the template ref.
                //   * Anything else (class ref, `InjectionToken`, array
                //     literal) flows through verbatim.
                let predicate_source = call
                    .arguments
                    .first()
                    .map(|arg| match arg {
                        Argument::StringLiteral(s) => format!("['{}']", s.value),
                        _ => {
                            let span = arg.span();
                            source[span.start as usize..span.end as usize].to_string()
                        }
                    })
                    .unwrap_or_else(|| "null".to_string());
                let opts = parse_signal_options(source, call, 1);
                out.queries.push(SignalQuerySpec {
                    property_name,
                    kind,
                    predicate_source,
                    read_source: opts.read,
                    is_static: opts.is_static,
                    descendants: opts.descendants,
                });
            }
        }
    }

    out
}

/// What a signal-factory callee resolved to. `required` carries through
/// for input/model so the codegen can emit a different shape later.
enum SignalFactory {
    Input { required: bool },
    Output,
    Model { required: bool },
    Query { kind: SignalQueryKind },
}

/// Classify a class-field call's callee against the known signal-API
/// factory names. Recognises bare identifiers (`input(...)`) and the
/// `.required` member form (`input.required(...)` etc.).
fn classify_signal_callee(callee: &Expression<'_>) -> Option<SignalFactory> {
    match callee {
        Expression::Identifier(id) => name_to_factory(id.name.as_str(), false),
        Expression::StaticMemberExpression(member) => {
            let Expression::Identifier(root) = &member.object else {
                return None;
            };
            if member.property.name.as_str() != "required" {
                return None;
            }
            name_to_factory(root.name.as_str(), true)
        }
        _ => None,
    }
}

/// Map a factory identifier to its [`SignalFactory`] variant. `dotted` is
/// `true` for the `.required` form — only valid on `input`, `model`,
/// `viewChild`, `contentChild` (the plural-query variants don't have a
/// `.required` API).
fn name_to_factory(name: &str, dotted: bool) -> Option<SignalFactory> {
    match (name, dotted) {
        ("input", false) => Some(SignalFactory::Input { required: false }),
        ("input", true) => Some(SignalFactory::Input { required: true }),
        ("output", false) => Some(SignalFactory::Output),
        ("model", false) => Some(SignalFactory::Model { required: false }),
        ("model", true) => Some(SignalFactory::Model { required: true }),
        ("viewChild", false) => Some(SignalFactory::Query {
            kind: SignalQueryKind::ViewChild,
        }),
        ("viewChild", true) => Some(SignalFactory::Query {
            kind: SignalQueryKind::ViewChild,
        }),
        ("viewChildren", false) => Some(SignalFactory::Query {
            kind: SignalQueryKind::ViewChildren,
        }),
        ("contentChild", false) => Some(SignalFactory::Query {
            kind: SignalQueryKind::ContentChild,
        }),
        ("contentChild", true) => Some(SignalFactory::Query {
            kind: SignalQueryKind::ContentChild,
        }),
        ("contentChildren", false) => Some(SignalFactory::Query {
            kind: SignalQueryKind::ContentChildren,
        }),
        _ => None,
    }
}

/// Options pulled from the second argument of a signal-API factory
/// call (`input(default, { ... })`, `viewChild(predicate, { ... })`).
/// Each field is `None` when absent from the options object so callers
/// can distinguish "user wrote `false`" from "user didn't write
/// anything" — that distinction matters for `descendants`, which has
/// kind-specific defaults that should only kick in when omitted.
#[derive(Debug, Default)]
struct SignalOptions {
    alias: Option<String>,
    transform: Option<String>,
    read: Option<String>,
    is_static: bool,
    descendants: Option<bool>,
}

/// Pull `alias`, `transform`, `read`, `static`, `descendants` out of an
/// options object argument at the given positional index, if it's an
/// object literal. Missing or non-object args produce defaults.
fn parse_signal_options(
    source: &str,
    call: &oxc_ast::ast::CallExpression<'_>,
    idx: usize,
) -> SignalOptions {
    let mut out = SignalOptions::default();

    let Some(arg) = call.arguments.get(idx) else {
        return out;
    };
    let Argument::ObjectExpression(opts) = arg else {
        return out;
    };

    for prop in &opts.properties {
        let ObjectPropertyKind::ObjectProperty(prop) = prop else {
            continue;
        };
        let key = match &prop.key {
            PropertyKey::StaticIdentifier(id) => id.name.as_str(),
            PropertyKey::StringLiteral(s) => s.value.as_str(),
            _ => continue,
        };
        match key {
            "alias" => {
                if let Expression::StringLiteral(s) = &prop.value {
                    out.alias = Some(s.value.to_string());
                }
            }
            "transform" => {
                let span = prop.value.span();
                out.transform = Some(source[span.start as usize..span.end as usize].to_string());
            }
            "read" => {
                let span = prop.value.span();
                out.read = Some(source[span.start as usize..span.end as usize].to_string());
            }
            "static" => {
                if let Expression::BooleanLiteral(b) = &prop.value {
                    out.is_static = b.value;
                }
            }
            "descendants" => {
                if let Expression::BooleanLiteral(b) = &prop.value {
                    out.descendants = Some(b.value);
                }
            }
            _ => {}
        }
    }

    out
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
    inline_styles: Vec<String>,
    style_urls: Vec<String>,
    animations_source: Option<String>,
    host_directives_source: Option<String>,
    change_detection: Option<u32>,
}

/// Extract metadata from the `@Component({...})` decorator argument.
fn extract_decorator_metadata(source: &str, decorator: &Decorator) -> NgcResult<DecoratorMetadata> {
    let empty = || DecoratorMetadata {
        selector: String::new(),
        template: None,
        template_url: None,
        standalone: false,
        imports_source: None,
        imports_identifiers: Vec::new(),
        styles_source: None,
        inline_styles: Vec::new(),
        style_urls: Vec::new(),
        animations_source: None,
        host_directives_source: None,
        change_detection: None,
    };

    let call = match &decorator.expression {
        Expression::CallExpression(call) => call,
        _ => return Ok(empty()),
    };

    let arg = match call.arguments.first() {
        Some(Argument::ObjectExpression(obj)) => obj,
        _ => return Ok(empty()),
    };

    let mut selector = String::new();
    let mut template = None;
    let mut template_url = None;
    let mut standalone = false;
    let mut imports_source = None;
    let mut imports_identifiers = Vec::new();
    let mut styles_source = None;
    let mut inline_styles: Vec<String> = Vec::new();
    let mut style_urls: Vec<String> = Vec::new();
    let mut animations_source = None;
    let mut host_directives_source = None;
    let mut change_detection = None;

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
                "styles" => {
                    let start = prop.value.span().start as usize;
                    let end = prop.value.span().end as usize;
                    if start < source.len() && end <= source.len() {
                        styles_source = Some(source[start..end].to_string());
                    }
                    // Also parse each element into a structured list so the
                    // preprocessor step can operate on raw content rather than
                    // the JS source text.
                    match &prop.value {
                        Expression::ArrayExpression(arr) => {
                            for elem in &arr.elements {
                                match elem {
                                    oxc_ast::ast::ArrayExpressionElement::TemplateLiteral(tpl)
                                        if tpl.expressions.is_empty() =>
                                    {
                                        let text: String = tpl
                                            .quasis
                                            .iter()
                                            .map(|q| q.value.raw.as_str())
                                            .collect();
                                        inline_styles.push(text);
                                    }
                                    oxc_ast::ast::ArrayExpressionElement::StringLiteral(s) => {
                                        inline_styles.push(s.value.to_string());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Expression::StringLiteral(s) => {
                            inline_styles.push(s.value.to_string());
                        }
                        Expression::TemplateLiteral(tpl) if tpl.expressions.is_empty() => {
                            let text: String =
                                tpl.quasis.iter().map(|q| q.value.raw.as_str()).collect();
                            inline_styles.push(text);
                        }
                        _ => {}
                    }
                }
                "styleUrl" => {
                    if let Expression::StringLiteral(s) = &prop.value {
                        style_urls.push(s.value.to_string());
                    }
                }
                "styleUrls" => {
                    if let Expression::ArrayExpression(arr) = &prop.value {
                        for elem in &arr.elements {
                            if let oxc_ast::ast::ArrayExpressionElement::StringLiteral(s) = elem {
                                style_urls.push(s.value.to_string());
                            }
                        }
                    } else if let Expression::StringLiteral(s) = &prop.value {
                        // Defensive: single string passed to styleUrls.
                        style_urls.push(s.value.to_string());
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
                "animations" => {
                    let start = prop.value.span().start as usize;
                    let end = prop.value.span().end as usize;
                    if start < source.len() && end <= source.len() {
                        animations_source = Some(source[start..end].to_string());
                    }
                }
                "hostDirectives" => {
                    let start = prop.value.span().start as usize;
                    let end = prop.value.span().end as usize;
                    if start < source.len() && end <= source.len() {
                        host_directives_source = Some(source[start..end].to_string());
                    }
                }
                "changeDetection" => {
                    // Recognise `ChangeDetectionStrategy.OnPush` / `Default`
                    // member references and lower them to the runtime
                    // numeric values (0 / 1). Anything else (a literal,
                    // an aliased import, a ternary) we just leave
                    // unresolved — the linker / runtime gets a missing
                    // `changeDetection` key in that case, which is
                    // identical behaviour to omitting the property.
                    if let Expression::StaticMemberExpression(member) = &prop.value {
                        change_detection = match member.property.name.as_str() {
                            "OnPush" => Some(0),
                            "Default" => Some(1),
                            _ => None,
                        };
                    } else if let Expression::NumericLiteral(n) = &prop.value {
                        change_detection = Some(n.value as u32);
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
        inline_styles,
        style_urls,
        animations_source,
        host_directives_source,
        change_detection,
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

    #[test]
    fn test_extract_host_listener_event_only() {
        let source = r#"import { Directive, HostListener } from '@angular/core';

@Directive({ selector: '[appResize]' })
export class ResizeDirective {
  @HostListener('window:resize')
  onResize() {}
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.host_listeners.len(), 1);
        assert_eq!(result.host_listeners[0].event, "window:resize");
        assert_eq!(result.host_listeners[0].handler_expression, "onResize()");
    }

    #[test]
    fn test_extract_host_listener_with_args() {
        let source = r#"import { Directive, HostListener } from '@angular/core';

@Directive({ selector: '[appClick]' })
export class ClickDirective {
  @HostListener('click', ['$event'])
  onClick($event: MouseEvent) {}
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.host_listeners.len(), 1);
        assert_eq!(result.host_listeners[0].event, "click");
        assert_eq!(
            result.host_listeners[0].handler_expression,
            "onClick($event)"
        );
    }

    #[test]
    fn test_extract_host_binding_bare() {
        let source = r#"import { Directive, HostBinding } from '@angular/core';

@Directive({ selector: '[appX]' })
export class XDirective {
  @HostBinding('disabled') isDisabled = false;
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.host_bindings.len(), 1);
        assert_eq!(result.host_bindings[0].target, "disabled");
        assert_eq!(result.host_bindings[0].property_name, "isDisabled");
    }

    #[test]
    fn test_extract_host_binding_class_attr_style() {
        let source = r#"import { Directive, HostBinding } from '@angular/core';

@Directive({ selector: '[appX]' })
export class XDirective {
  @HostBinding('class.active') isActive = true;
  @HostBinding('attr.aria-label') label = 'x';
  @HostBinding('style.width.px') width = 100;
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.host_bindings.len(), 3);
        let targets: Vec<&str> = result
            .host_bindings
            .iter()
            .map(|b| b.target.as_str())
            .collect();
        assert!(targets.contains(&"class.active"));
        assert!(targets.contains(&"attr.aria-label"));
        assert!(targets.contains(&"style.width.px"));
    }

    #[test]
    fn test_extract_host_binding_no_arg_uses_property_name() {
        let source = r#"import { Directive, HostBinding } from '@angular/core';

@Directive({ selector: '[appX]' })
export class XDirective {
  @HostBinding() id = 'host';
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.host_bindings.len(), 1);
        assert_eq!(result.host_bindings[0].target, "id");
        assert_eq!(result.host_bindings[0].property_name, "id");
    }

    #[test]
    fn test_extract_host_binding_on_getter() {
        // Angular accepts @HostBinding on a getter — the value is a computed
        // expression rather than a plain field.
        let source = r#"import { Directive, HostBinding } from '@angular/core';

@Directive({ selector: '[appX]' })
export class XDirective {
  @HostBinding('attr.title') get title() { return 'tip'; }
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("should extract")
            .expect("should find directive");
        assert_eq!(result.host_bindings.len(), 1);
        assert_eq!(result.host_bindings[0].target, "attr.title");
        assert_eq!(result.host_bindings[0].property_name, "title");
    }

    #[test]
    fn test_component_extracts_host_decorators() {
        let source = r#"import { Component, HostListener, HostBinding } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  @HostBinding('class.dark') isDark = false;
  @HostListener('window:resize') onResize() {}
}
"#;
        let result = extract_component(source, &test_path())
            .expect("should extract")
            .expect("should find component");
        assert_eq!(result.host_listeners.len(), 1);
        assert_eq!(result.host_bindings.len(), 1);
        assert_eq!(result.host_listeners[0].event, "window:resize");
        assert_eq!(result.host_bindings[0].target, "class.dark");
    }

    /// `name = input(...)` should land in `signal_inputs` with no alias
    /// and `is_required = false`. We're verifying the call-shape match
    /// (bare identifier `input`) more than the option parsing here —
    /// the option-parsing path is exercised separately.
    #[test]
    fn test_extract_signal_input_basic() {
        let source = r#"import { Component, input } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  name = input('default');
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_inputs.len(), 1);
        assert_eq!(result.signal_inputs[0].property_name, "name");
        assert!(result.signal_inputs[0].alias.is_none());
        assert!(!result.signal_inputs[0].is_required);
    }

    /// `input.required()` flows through the member-call branch of
    /// `classify_signal_callee` and must mark the spec as required so
    /// the codegen can drop the default-value plumbing.
    #[test]
    fn test_extract_signal_input_required() {
        let source = r#"import { Component, input } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  name = input.required<string>();
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_inputs.len(), 1);
        assert!(result.signal_inputs[0].is_required);
    }

    /// `input(0, { alias: 'pub', transform: trimString })` must surface
    /// both the alias (string literal) AND the transform reference
    /// (preserved verbatim as raw source so the codegen emits a
    /// callable expression, not a stringified value).
    #[test]
    fn test_extract_signal_input_with_alias_and_transform() {
        let source = r#"import { Component, input } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  name = input('def', { alias: 'pub', transform: trimString });
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_inputs.len(), 1);
        assert_eq!(result.signal_inputs[0].alias.as_deref(), Some("pub"));
        assert_eq!(
            result.signal_inputs[0].transform_source.as_deref(),
            Some("trimString")
        );
    }

    /// `output<T>()` lands in `signal_outputs`. No options — bare call.
    #[test]
    fn test_extract_signal_output() {
        let source = r#"import { Component, output } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  changed = output<string>();
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_outputs.len(), 1);
        assert_eq!(result.signal_outputs[0].property_name, "changed");
        assert!(result.signal_outputs[0].alias.is_none());
    }

    /// `model<T>()` is a separate member kind — must NOT be conflated
    /// with `input()` / `output()`. The codegen needs to know it's a
    /// model so it can emit the paired `<name>Change` output.
    #[test]
    fn test_extract_signal_model() {
        let source = r#"import { Component, model } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  value = model<string>('');
  required = model.required<number>();
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_models.len(), 2);
        assert_eq!(result.signal_models[0].property_name, "value");
        assert!(!result.signal_models[0].is_required);
        assert_eq!(result.signal_models[1].property_name, "required");
        assert!(result.signal_models[1].is_required);
        // Models must not double-count as inputs or outputs in the
        // extracted lists — codegen merges them into the maps itself.
        assert!(result.signal_inputs.is_empty());
        assert!(result.signal_outputs.is_empty());
    }

    /// Each of the four query factories must classify into the right
    /// `SignalQueryKind` so the codegen splits them into the correct
    /// `viewQuery` / `contentQueries` function. The `.required` member
    /// form on `viewChild` / `contentChild` should map to the same
    /// kind as the bare form (required-ness only changes the
    /// runtime-level signal default).
    #[test]
    fn test_extract_signal_queries_all_kinds() {
        let source = r#"import { Component, viewChild, viewChildren, contentChild, contentChildren } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  v = viewChild<string>('ref');
  vs = viewChildren(SomeCmp);
  c = contentChild.required<SomeDir>(SomeDir);
  cs = contentChildren(SomeDir, { descendants: true, read: ElementRef });
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_queries.len(), 4);

        let by_name = |n: &str| -> &SignalQuerySpec {
            result
                .signal_queries
                .iter()
                .find(|q| q.property_name == n)
                .expect("kind present")
        };
        assert!(matches!(by_name("v").kind, SignalQueryKind::ViewChild));
        assert!(matches!(by_name("vs").kind, SignalQueryKind::ViewChildren));
        assert!(matches!(by_name("c").kind, SignalQueryKind::ContentChild));
        assert!(matches!(
            by_name("cs").kind,
            SignalQueryKind::ContentChildren
        ));
        // Bare string predicate must be wrapped in an array — the
        // runtime distinguishes `['ref']` (template ref) from `'ref'`
        // (provider token).
        assert_eq!(by_name("v").predicate_source, "['ref']");
        assert_eq!(by_name("cs").read_source.as_deref(), Some("ElementRef"));
    }

    /// `@Directive` should pick up the same signal-API fields that
    /// `@Component` does — the extraction logic is shared. Confirms
    /// the directive extractor calls `extract_signal_members` too.
    #[test]
    fn test_extract_directive_signal_inputs() {
        let source = r#"import { Directive, input, output } from '@angular/core';

@Directive({ selector: '[appX]', standalone: true })
export class XDirective {
  value = input<string>('');
  changed = output<string>();
}
"#;
        let result = extract_directive(source, &test_path())
            .expect("extract")
            .expect("found");
        assert_eq!(result.signal_inputs.len(), 1);
        assert_eq!(result.signal_inputs[0].property_name, "value");
        assert_eq!(result.signal_outputs.len(), 1);
        assert_eq!(result.signal_outputs[0].property_name, "changed");
    }

    /// A class field initialised by an unrelated function call like
    /// `someUtil(...)` must NOT be picked up as a signal member.
    /// Regression guard: the classifier should stop at the bare
    /// `input` / `output` / `model` / `view*` / `content*` identifiers
    /// and ignore everything else.
    #[test]
    fn test_extract_signal_ignores_unrelated_calls() {
        let source = r#"import { Component } from '@angular/core';

@Component({ selector: 'app-x', standalone: true, template: '' })
export class XComponent {
  data = computeSomething();
  count = 0;
}
"#;
        let result = extract_component(source, &test_path())
            .expect("extract")
            .expect("found");
        assert!(result.signal_inputs.is_empty());
        assert!(result.signal_outputs.is_empty());
        assert!(result.signal_models.is_empty());
        assert!(result.signal_queries.is_empty());
    }
}
