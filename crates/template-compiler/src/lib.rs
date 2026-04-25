//! Angular Ivy compiler for ngc-rs.
//!
//! AOT-compiles Angular decorators (`@Component`, `@Injectable`, `@Directive`,
//! `@Pipe`, `@NgModule`) to Ivy static fields. Parses template HTML with pest,
//! generates Ivy codegen, and rewrites TypeScript source to replace decorators
//! with static Ivy metadata.

mod ast;
mod codegen;
mod directive_codegen;
mod extract;
mod factory_codegen;
pub mod host_codegen;
pub mod i18n;
mod injectable_codegen;
mod ng_module_codegen;
mod parser;
mod pipe_codegen;
pub mod preprocessor;
mod rewrite;
mod selector;

pub use parser::parse_template;

use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use rayon::prelude::*;
use tracing::debug;

pub use preprocessor::StyleLanguage;

/// Context needed to preprocess component styles.
///
/// Threaded through `compile_all_decorators_with_styles` so that SCSS / Less /
/// Stylus subprocesses can locate `node_modules` (via `project_root`) and so
/// that inline `styles: [\`...\`]` literals are interpreted against the
/// configured `inlineStyleLanguage` from `angular.json`.
#[derive(Debug, Clone)]
pub struct StyleContext {
    /// Directory that contains `node_modules` — usually the project root
    /// (the directory containing `angular.json`).
    pub project_root: PathBuf,
    /// `inlineStyleLanguage` from `angular.json`, applied to bodies of
    /// `styles: [\`...\`]` array entries. File-based `styleUrl`/`styleUrls`
    /// entries always derive their language from the file extension.
    pub inline_style_language: StyleLanguage,
}

impl Default for StyleContext {
    fn default() -> Self {
        Self {
            project_root: PathBuf::from("."),
            inline_style_language: StyleLanguage::Css,
        }
    }
}

/// Lightweight metadata for template compilation without `ExtractedComponent`.
///
/// Used by the Angular linker to compile templates from `ɵɵngDeclareComponent`
/// metadata without needing decorator extraction.
#[derive(Debug, Clone)]
pub struct TemplateMetadata {
    /// The component class name.
    pub class_name: String,
    /// The component selector.
    pub selector: String,
    /// Whether the component is standalone.
    pub standalone: bool,
    /// Raw source text of the imports array.
    pub imports_source: Option<String>,
    /// Raw source text of the styles array.
    pub styles_source: Option<String>,
}

/// Result of compiling just the template function (without full defineComponent).
#[derive(Debug, Clone)]
pub struct TemplateFnOutput {
    /// The template function code: `function Name_Template(rf, ctx) { ... }`.
    pub template_function: String,
    /// Number of element/text slots used.
    pub decls: u32,
    /// Number of binding variables used.
    pub vars: u32,
    /// Child template functions (for @if, @for, @switch blocks).
    pub child_template_functions: Vec<String>,
    /// Set of Ivy runtime symbols needed from `@angular/core`.
    pub ivy_imports: std::collections::BTreeSet<String>,
    /// Static attribute arrays for the consts property of defineComponent.
    pub consts: Vec<String>,
}

/// Compile a template string into a standalone template function.
///
/// This is the public API used by the Angular linker to compile templates
/// from `ɵɵngDeclareComponent` calls in npm packages.
pub fn generate_template_fn(
    template: &str,
    meta: &TemplateMetadata,
    file_path: &Path,
) -> NgcResult<TemplateFnOutput> {
    // Parse the template
    let template_ast = parser::parse_template(template, file_path)?;

    // Convert to ExtractedComponent for codegen compatibility
    let extracted = extract::ExtractedComponent {
        class_name: meta.class_name.clone(),
        selector: meta.selector.clone(),
        template: Some(template.to_string()),
        template_url: None,
        standalone: meta.standalone,
        imports_source: meta.imports_source.clone(),
        imports_identifiers: Vec::new(),
        decorator_span: (0, 0),
        class_body_start: 0,
        export_keyword_start: None,
        class_keyword_start: 0,
        angular_core_import_span: None,
        other_angular_core_imports: Vec::new(),
        styles_source: meta.styles_source.clone(),
        inline_styles: Vec::new(),
        style_urls: Vec::new(),
        input_properties: Vec::new(),
        host_listeners: Vec::new(),
        host_bindings: Vec::new(),
        animations_source: None,
    };

    let ivy_output = codegen::generate_ivy(&extracted, &template_ast)?;

    // Extract just the template function from the defineComponent code
    let template_fn = extract_template_fn_from_ivy(&ivy_output, &meta.class_name);

    // Strip TypeScript type annotations — the codegen produces TS (`rf: number, ctx: ClassName`)
    // but the linker outputs into .mjs files which must be plain JavaScript.
    let template_fn = strip_ts_annotations(&template_fn, &meta.class_name);

    // Extract decls and vars from the defineComponent code
    let (decls, vars) = extract_decls_vars_from_ivy(&ivy_output);

    // Strip TS annotations from child template functions too
    let child_fns = ivy_output
        .child_template_functions
        .iter()
        .map(|f| strip_ts_annotations(f, &meta.class_name))
        .collect();

    Ok(TemplateFnOutput {
        template_function: template_fn,
        decls,
        vars,
        child_template_functions: child_fns,
        ivy_imports: ivy_output.ivy_imports,
        consts: ivy_output.consts,
    })
}

/// Extract the template function from the IvyOutput's defineComponent code.
fn extract_template_fn_from_ivy(ivy: &codegen::IvyOutput, class_name: &str) -> String {
    let dc = ivy.static_fields.first().map(|s| s.as_str()).unwrap_or("");
    let template_marker = format!("template: function {class_name}_Template");

    if let Some(start) = dc.find(&template_marker) {
        // Find the function start
        let fn_start = start + "template: ".len();
        // Find the matching closing brace by counting braces
        let remaining = &dc[fn_start..];
        let mut depth = 0;
        let mut end = 0;
        for (i, ch) in remaining.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > 0 {
            return remaining[..end].to_string();
        }
    }

    // Fallback: empty template
    format!("function {class_name}_Template(rf, ctx) {{}}")
}

/// Extract decls and vars from the IvyOutput's defineComponent code.
fn extract_decls_vars_from_ivy(ivy: &codegen::IvyOutput) -> (u32, u32) {
    let dc = ivy.static_fields.first().map(|s| s.as_str()).unwrap_or("");

    let decls = extract_number_prop(dc, "decls: ").unwrap_or(0);
    let vars = extract_number_prop(dc, "vars: ").unwrap_or(0);

    (decls, vars)
}

/// Extract a numeric property value from generated code.
fn extract_number_prop(code: &str, prefix: &str) -> Option<u32> {
    let start = code.find(prefix)? + prefix.len();
    let remaining = &code[start..];
    let end = remaining.find(|c: char| !c.is_ascii_digit())?;
    remaining[..end].parse().ok()
}

/// Strip TypeScript type annotations from generated template functions.
///
/// The codegen produces TypeScript-flavored output like `(rf: number, ctx: ClassName)`
/// which is valid TS but not valid JS. For the linker (which outputs into `.mjs` files),
/// we need to strip these annotations.
fn strip_ts_annotations(code: &str, class_name: &str) -> String {
    code.replace(&format!("rf: number, ctx: {class_name}"), "rf, ctx")
        .replace("rf: number, ctx: any", "rf, ctx")
        .replace("t: any", "t")
}

/// Result of template compilation for a single file.
#[derive(Debug, Clone)]
pub struct CompiledFile {
    /// The original source file path.
    pub source_path: PathBuf,
    /// The rewritten TypeScript source (or original if no @Component found).
    pub source: String,
    /// Whether Ivy compilation was applied.
    pub compiled: bool,
    /// Whether JIT fallback was used (decorator left as-is).
    pub jit_fallback: bool,
}

/// Compile all Angular decorators in the given TypeScript source files.
///
/// Handles `@Component`, `@Injectable`, `@Directive`, `@Pipe`, and `@NgModule`.
/// For each file, extracts Angular decorators, generates Ivy instructions, and
/// rewrites the source to replace decorators with static Ivy metadata.
/// Files without Angular decorators are returned unchanged.
///
/// Files are processed in parallel using rayon.
///
/// Uses a default style context (no preprocessing). Callers that need SCSS /
/// Less / Stylus support should use [`compile_all_decorators_with_styles`].
pub fn compile_all_decorators(files: &[PathBuf]) -> NgcResult<Vec<CompiledFile>> {
    compile_all_decorators_with_styles(files, &StyleContext::default())
}

/// Variant of [`compile_all_decorators`] that preprocesses component styles.
pub fn compile_all_decorators_with_styles(
    files: &[PathBuf],
    style_ctx: &StyleContext,
) -> NgcResult<Vec<CompiledFile>> {
    let results: Vec<NgcResult<CompiledFile>> = files
        .par_iter()
        .map(|file_path| {
            let source = std::fs::read_to_string(file_path).map_err(|e| NgcError::Io {
                path: file_path.clone(),
                source: e,
            })?;

            compile_file(&source, file_path, style_ctx)
        })
        .collect();

    results.into_iter().collect()
}

/// Backward-compatible alias for `compile_all_decorators`.
pub fn compile_templates(files: &[PathBuf]) -> NgcResult<Vec<CompiledFile>> {
    compile_all_decorators(files)
}

/// Compile a single TypeScript source file, handling all Angular decorator types.
///
/// Tries `@Component` first, then falls through to `@Injectable`, `@Directive`,
/// `@Pipe`, and `@NgModule`. Returns the source unchanged if no Angular decorator
/// is found.
fn compile_file(
    source: &str,
    file_path: &Path,
    style_ctx: &StyleContext,
) -> NgcResult<CompiledFile> {
    // Try @Component first (most complex, has template compilation)
    let component_result = compile_component_with_styles(source, file_path, style_ctx)?;
    if component_result.compiled || component_result.jit_fallback {
        return Ok(component_result);
    }

    // Try @Injectable
    if let Some(extracted) = extract::extract_injectable(source, file_path)? {
        let ivy_output = injectable_codegen::generate_injectable_ivy(&extracted)?;
        let rewritten = rewrite::rewrite_source_generic(source, &extracted.common, &ivy_output)?;
        debug!(path = %file_path.display(), "compiled @Injectable to Ivy");
        return Ok(CompiledFile {
            source_path: file_path.to_path_buf(),
            source: rewritten,
            compiled: true,
            jit_fallback: false,
        });
    }

    // Try @Directive
    if let Some(extracted) = extract::extract_directive(source, file_path)? {
        let ivy_output = directive_codegen::generate_directive_ivy(&extracted)?;
        let rewritten = rewrite::rewrite_source_generic(source, &extracted.common, &ivy_output)?;
        debug!(path = %file_path.display(), "compiled @Directive to Ivy");
        return Ok(CompiledFile {
            source_path: file_path.to_path_buf(),
            source: rewritten,
            compiled: true,
            jit_fallback: false,
        });
    }

    // Try @Pipe
    if let Some(extracted) = extract::extract_pipe(source, file_path)? {
        let ivy_output = pipe_codegen::generate_pipe_ivy(&extracted)?;
        let rewritten = rewrite::rewrite_source_generic(source, &extracted.common, &ivy_output)?;
        debug!(path = %file_path.display(), "compiled @Pipe to Ivy");
        return Ok(CompiledFile {
            source_path: file_path.to_path_buf(),
            source: rewritten,
            compiled: true,
            jit_fallback: false,
        });
    }

    // Try @NgModule
    if let Some(extracted) = extract::extract_ng_module(source, file_path)? {
        let ivy_output = ng_module_codegen::generate_ng_module_ivy(&extracted)?;
        let rewritten = rewrite::rewrite_source_generic(source, &extracted.common, &ivy_output)?;
        debug!(path = %file_path.display(), "compiled @NgModule to Ivy");
        return Ok(CompiledFile {
            source_path: file_path.to_path_buf(),
            source: rewritten,
            compiled: true,
            jit_fallback: false,
        });
    }

    // No Angular decorator found
    Ok(CompiledFile {
        source_path: file_path.to_path_buf(),
        source: source.to_string(),
        compiled: false,
        jit_fallback: false,
    })
}

/// Resolve `styleUrl`/`styleUrls` from disk, preprocess them plus any inline
/// `styles: [\`...\`]` entries according to `style_ctx.inline_style_language`,
/// and rewrite `extracted.styles_source` to a template-literal array of
/// compiled CSS.
///
/// Has no effect for components that only use plain CSS and have no
/// `styleUrl`/`styleUrls` — the existing raw `styles_source` flows through
/// unchanged and the PostCSS-style %COMP% scoping works as before.
fn preprocess_component_styles(
    extracted: &mut extract::ExtractedComponent,
    file_path: &Path,
    style_ctx: &StyleContext,
) -> NgcResult<()> {
    let needs_url_resolution = !extracted.style_urls.is_empty();
    let inline_needs_preproc = style_ctx.inline_style_language != StyleLanguage::Css
        && !extracted.inline_styles.is_empty();

    if !needs_url_resolution && !inline_needs_preproc {
        return Ok(());
    }

    let base_dir = file_path.parent().unwrap_or(Path::new("."));
    let mut compiled: Vec<String> =
        Vec::with_capacity(extracted.inline_styles.len() + extracted.style_urls.len());

    // Preprocess inline styles under inlineStyleLanguage.
    for src in &extracted.inline_styles {
        let css = preprocessor::preprocess_style(
            src,
            style_ctx.inline_style_language,
            &style_ctx.project_root,
            file_path,
        )?;
        compiled.push(css);
    }

    // Resolve + preprocess each styleUrl/styleUrls entry.
    for url in &extracted.style_urls {
        let path = base_dir.join(url);
        let content = std::fs::read_to_string(&path).map_err(|e| NgcError::Io {
            path: path.clone(),
            source: e,
        })?;
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let language = StyleLanguage::from_extension(ext);
        let css =
            preprocessor::preprocess_style(&content, language, &style_ctx.project_root, &path)?;
        compiled.push(css);
    }

    // Synthesize a JS array literal of backtick-quoted CSS strings. Any
    // existing backticks in the CSS are escaped so the emitted source parses.
    let mut arr = String::from("[");
    for (i, css) in compiled.iter().enumerate() {
        if i > 0 {
            arr.push_str(", ");
        }
        arr.push('`');
        for ch in css.chars() {
            match ch {
                '`' => arr.push_str("\\`"),
                '\\' => arr.push_str("\\\\"),
                '$' => arr.push_str("\\$"),
                _ => arr.push(ch),
            }
        }
        arr.push('`');
    }
    arr.push(']');
    extracted.styles_source = Some(arr);
    Ok(())
}

/// Compile a single TypeScript source string containing an Angular component.
///
/// If the source contains an `@Component` decorator with an inline `template`,
/// parses the template, generates Ivy instructions, and rewrites the source.
/// If no `@Component` is found, returns the source unchanged.
///
/// Uses a default style context (no preprocessing). For SCSS / Less / Stylus
/// component styles, use [`compile_component_with_styles`].
pub fn compile_component(source: &str, file_path: &Path) -> NgcResult<CompiledFile> {
    compile_component_with_styles(source, file_path, &StyleContext::default())
}

/// Extract translatable i18n messages from a single component file.
///
/// Parses the component's inline `template:` (or external `templateUrl`),
/// walks the template AST, and returns one `ExtractedI18nMessage` per
/// `i18n` element / `i18n-<attr>` marker found. Files that contain no
/// `@Component` decorator return an empty list.
pub fn extract_i18n_from_file(file_path: &Path) -> NgcResult<Vec<i18n::ExtractedI18nMessage>> {
    let source = std::fs::read_to_string(file_path).map_err(|e| NgcError::Io {
        path: file_path.to_path_buf(),
        source: e,
    })?;
    let extracted = match extract::extract_component(&source, file_path)? {
        Some(ext) => ext,
        None => return Ok(Vec::new()),
    };
    let resolved_template;
    let template_source = if let Some(ref t) = extracted.template {
        t.as_str()
    } else if let Some(ref url) = extracted.template_url {
        let base_dir = file_path.parent().unwrap_or(Path::new("."));
        let html_path = base_dir.join(url);
        resolved_template = std::fs::read_to_string(&html_path).map_err(|e| NgcError::Io {
            path: html_path,
            source: e,
        })?;
        &resolved_template
    } else {
        return Ok(Vec::new());
    };
    let nodes = parser::parse_template(template_source, file_path)?;
    Ok(i18n::extract_messages(&nodes))
}

/// Variant of [`compile_component`] that preprocesses SCSS / Less / Stylus
/// component styles into CSS before codegen emits the `styles:` array.
pub fn compile_component_with_styles(
    source: &str,
    file_path: &Path,
    style_ctx: &StyleContext,
) -> NgcResult<CompiledFile> {
    let mut extracted = match extract::extract_component(source, file_path)? {
        Some(ext) => ext,
        None => {
            return Ok(CompiledFile {
                source_path: file_path.to_path_buf(),
                source: source.to_string(),
                compiled: false,
                jit_fallback: false,
            });
        }
    };

    // Check for JIT fallback triggers
    if extracted.needs_jit_fallback() {
        tracing::warn!(
            path = %file_path.display(),
            "template contains unsupported constructs, using JIT fallback"
        );
        return Ok(CompiledFile {
            source_path: file_path.to_path_buf(),
            source: source.to_string(),
            compiled: false,
            jit_fallback: true,
        });
    }

    // Resolve + preprocess any non-CSS component styles. Rewrites
    // `extracted.styles_source` to a template-literal array of compiled CSS so
    // that codegen + CSS scoping proceed unchanged.
    preprocess_component_styles(&mut extracted, file_path, style_ctx)?;

    // Resolve template source: inline template or external templateUrl
    let resolved_template;
    let template_source = if let Some(ref t) = extracted.template {
        t.as_str()
    } else if let Some(ref url) = extracted.template_url {
        let base_dir = file_path.parent().unwrap_or(Path::new("."));
        let html_path = base_dir.join(url);
        resolved_template = std::fs::read_to_string(&html_path).map_err(|e| NgcError::Io {
            path: html_path,
            source: e,
        })?;
        &resolved_template
    } else {
        return Ok(CompiledFile {
            source_path: file_path.to_path_buf(),
            source: source.to_string(),
            compiled: false,
            jit_fallback: false,
        });
    };

    // Parse the template
    let template_ast = parser::parse_template(template_source, file_path)?;

    // Generate Ivy code
    let ivy_output = codegen::generate_ivy(&extracted, &template_ast)?;

    // Rewrite the source
    let rewritten = rewrite::rewrite_source(source, &extracted, &ivy_output)?;

    debug!(path = %file_path.display(), "compiled template to Ivy");

    Ok(CompiledFile {
        source_path: file_path.to_path_buf(),
        source: rewritten,
        compiled: true,
        jit_fallback: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_i18n_from_file_collects_messages() {
        use std::io::Write;
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-x',
  standalone: true,
  template: '<h1 i18n="@@intro">Hello</h1><img alt="Hi" i18n-alt="@@alt" />',
})
export class XComponent {}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.component.ts");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(source.as_bytes()).expect("write");
        drop(f);

        let messages = extract_i18n_from_file(&path).expect("extract");
        assert_eq!(messages.len(), 2);
        let ids: Vec<&str> = messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert!(ids.contains(&"intro"));
        assert!(ids.contains(&"alt"));
    }

    #[test]
    fn test_compile_and_transform_roundtrip() {
        let source = "import { Component, Input } from '@angular/core';\n\n@Component({\n  selector: 'app-test',\n  standalone: true,\n  template: '<div [class]=\"cls\"><span>Hi</span></div>',\n})\nexport class TestComponent {\n  @Input() cls = '';\n}\n".to_string();
        let path = PathBuf::from("test.component.ts");
        let result = compile_component(&source, &path).expect("should compile");
        assert!(result.compiled, "should be compiled");

        // Verify the compiled source can be parsed by oxc
        let js = ngc_ts_transform::transform_source(&result.source, "test.component.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
    }

    #[test]
    fn test_complex_component_roundtrip() {
        // Exact reproduction of SidenavComponent patterns including:
        // - HTML comments, multi-line attribute bindings, pipes in ternary sub-expressions
        // - non-null assertions, complex class/style/attr bindings, routerLink directives
        // - CSS styles in template literal array
        let source = r#"import { Component, DestroyRef, inject, OnInit, signal } from '@angular/core';
import { RouterModule } from '@angular/router';
import { TranslateModule } from '@ngx-translate/core';

@Component({
  selector: 'app-side-nav',
  standalone: true,
  imports: [RouterModule, TranslateModule],
  template: `
    <!-- Mobile Overlay Backdrop -->
    @if (mobileMenu.isMobileMenuOpen() && auth.token) {
      <div
        class="fixed inset-0 bg-neutral-900/60 backdrop-blur-sm z-40 md:hidden animate-fade-in"
        (click)="mobileMenu.close()"
        aria-hidden="true"
      ></div>
    }

    <nav
      class="sidebar"
      [class.w-64]="!isCollapsed || mobileMenu.isMobileMenuOpen()"
      [class.w-20]="isCollapsed && !mobileMenu.isMobileMenuOpen()"
      [class.translate-x-0]="mobileMenu.isMobileMenuOpen()"
      [hidden]="!auth.token"
    >
      <div class="flex items-center justify-between p-4">
        <div [class.hidden]="isCollapsed && !mobileMenu.isMobileMenuOpen()">
          <span class="text-xl font-bold">Treasr</span>
        </div>

        @if (isCollapsed && !mobileMenu.isMobileMenuOpen()) {
          <div class="mx-auto">
            <span class="material-icons text-white text-xl">trending_up</span>
          </div>
        }

        <button
          type="button"
          class="p-2 cursor-pointer hidden md:block"
          [class.ml-auto]="!isCollapsed"
          [class.hidden]="isCollapsed && !mobileMenu.isMobileMenuOpen()"
          (click)="toggleSidebar()"
          [attr.aria-expanded]="!isCollapsed"
          [attr.aria-label]="
            isCollapsed ? 'Expand sidebar' : 'Collapse sidebar'
          "
        >
          <span
            class="material-icons"
            [style.transform]="isCollapsed ? 'rotate(180deg)' : 'rotate(0deg)'"
          >
            chevron_left
          </span>
        </button>
      </div>

      @if (subscription() && (!isCollapsed || mobileMenu.isMobileMenuOpen())) {
        <a
          routerLink="/billing"
          (click)="mobileMenu.close()"
          class="mx-3 mb-4 px-3 py-2.5 rounded-xl block cursor-pointer"
          [class]="getBadgeClass(subscription()!.tier, subscription()!.status)"
          [attr.title]="
            subscription()!.tier === 'free'
              ? ('NAV.UPGRADE' | translate)
              : ('NAV.BILLING' | translate)
          "
        >
          <div class="flex items-center justify-between">
            <div>
              <div class="text-xs font-semibold">
                {{ getTierDisplayName(subscription()!.tier) | translate }}
              </div>
              @if (subscription()!.status === 'trialing') {
                <div class="text-xs opacity-80">
                  {{ 'NAV.TRIAL' | translate }}
                </div>
              }
              @if (subscription()!.status === 'canceled') {
                <div class="text-xs opacity-80">
                  {{ 'NAV.EXPIRING' | translate }}
                </div>
              }
            </div>
            @if (subscription()!.tier === 'free') {
              <span class="material-icons text-sm opacity-80">arrow_upward</span>
            } @else {
              <span class="material-icons text-sm opacity-60">chevron_right</span>
            }
          </div>
        </a>
      }

      <ul class="flex-1 space-y-1 px-3">
        <li>
          <a
            routerLink="/portfolio"
            (click)="mobileMenu.close()"
            class="sidebar-nav-item"
            routerLinkActive="active"
            [routerLinkActiveOptions]="{ exact: true }"
            [class.justify-center]="isCollapsed && !mobileMenu.isMobileMenuOpen()"
            title="Portfolio"
          >
            <span class="material-icons text-xl">account_balance</span>
            <span
              class="font-medium"
              [class.hidden]="isCollapsed && !mobileMenu.isMobileMenuOpen()"
            >{{ 'NAV.PORTFOLIO' | translate }}</span>
          </a>
        </li>
      </ul>

      <div class="px-3 pb-2">
        <div
          class="border-t my-3"
          [class.hidden]="isCollapsed && !mobileMenu.isMobileMenuOpen()"
        ></div>
        <div
          class="px-3 mb-2 text-xs"
          [class.hidden]="isCollapsed && !mobileMenu.isMobileMenuOpen()"
        >
          {{ 'NAV.ACCOUNT' | translate }}
        </div>
      </div>

      @if (isCollapsed && !mobileMenu.isMobileMenuOpen()) {
        <div class="px-3 pb-4">
          <button
            type="button"
            class="w-full p-2 cursor-pointer flex items-center justify-center"
            (click)="toggleSidebar()"
            aria-label="Expand sidebar"
          >
            <span class="material-icons">chevron_right</span>
          </button>
        </div>
      }
    </nav>
  `,
  styles: [
    `
      .sidebar {
        background: linear-gradient(180deg, #f8fafc 0%, #f1f5f9 100%);
        border-right: 1px solid rgba(0, 0, 0, 0.08);
      }
      .sidebar-nav-item {
        display: flex;
        align-items: center;
        gap: 12px;
        padding: 12px 16px;
      }
    `,
  ],
})
export class SidenavComponent implements OnInit {
  isCollapsed = false;
  subscription = signal<any>(null);

  protected auth = inject(AuthService);
  protected mobileMenu = inject(MobileMenuService);

  ngOnInit() {}
  toggleSidebar() { this.isCollapsed = !this.isCollapsed; }
  getTierDisplayName(tier: string): string { return tier; }
  getBadgeClass(tier: string, status: string): string { return ''; }
}
"#;
        let path = PathBuf::from("test.component.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled, "should be compiled");
        assert!(
            !result.source.contains("@Component"),
            "decorator should be removed"
        );

        // The compiled source must be parseable by oxc ts-transform
        let js = ngc_ts_transform::transform_source(&result.source, "test.component.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
        let js = js.unwrap();
        assert!(js.contains("\u{0275}cmp"), "ɵcmp should survive transform");
    }

    #[test]
    fn test_injectable_roundtrip() {
        let source = r#"import { Injectable } from '@angular/core';

@Injectable({ providedIn: 'root' })
export class AuthService {
  isLoggedIn = false;
}
"#;
        let path = PathBuf::from("auth.service.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled, "should be compiled");
        assert!(result.source.contains("\u{0275}prov"));
        assert!(result.source.contains("\u{0275}\u{0275}defineInjectable"));
        assert!(!result.source.contains("@Injectable"));

        let js = ngc_ts_transform::transform_source(&result.source, "auth.service.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
    }

    #[test]
    fn test_injectable_with_deps_roundtrip() {
        let source = r#"import { Injectable } from '@angular/core';
import { HttpClient } from '@angular/common/http';
import { Router } from '@angular/router';

@Injectable({ providedIn: 'root' })
export class DataService {
  constructor(private http: HttpClient, private router: Router) {}
}
"#;
        let path = PathBuf::from("data.service.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled);
        assert!(result.source.contains("\u{0275}\u{0275}inject(HttpClient)"));
        assert!(result.source.contains("\u{0275}\u{0275}inject(Router)"));

        let js = ngc_ts_transform::transform_source(&result.source, "data.service.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
    }

    #[test]
    fn test_directive_roundtrip() {
        let source = r#"import { Directive, ElementRef } from '@angular/core';

@Directive({
  selector: '[appHighlight]',
  standalone: true
})
export class HighlightDirective {
  constructor(private el: ElementRef) {}
}
"#;
        let path = PathBuf::from("highlight.directive.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled);
        assert!(result.source.contains("\u{0275}dir"));
        assert!(result.source.contains("\u{0275}\u{0275}defineDirective"));
        assert!(result.source.contains("\u{0275}\u{0275}inject(ElementRef)"));
        assert!(!result.source.contains("@Directive"));

        let js = ngc_ts_transform::transform_source(&result.source, "highlight.directive.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
    }

    #[test]
    fn test_pipe_roundtrip() {
        let source = r#"import { Pipe, PipeTransform } from '@angular/core';

@Pipe({
  name: 'dateFormat',
  standalone: true
})
export class DateFormatPipe implements PipeTransform {
  transform(value: any): string { return ''; }
}
"#;
        let path = PathBuf::from("date-format.pipe.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled);
        assert!(result.source.contains("\u{0275}pipe"));
        assert!(result.source.contains("\u{0275}\u{0275}definePipe"));
        assert!(result.source.contains("name: 'dateFormat'"));
        assert!(!result.source.contains("@Pipe"));

        let js = ngc_ts_transform::transform_source(&result.source, "date-format.pipe.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
    }

    #[test]
    fn test_ng_module_roundtrip() {
        let source = r#"import { NgModule } from '@angular/core';

@NgModule({
  declarations: [AppComponent],
  imports: [CommonModule],
  bootstrap: [AppComponent]
})
export class AppModule {}
"#;
        let path = PathBuf::from("app.module.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled);
        assert!(result.source.contains("\u{0275}mod"));
        assert!(result.source.contains("\u{0275}inj"));
        assert!(result.source.contains("\u{0275}\u{0275}defineNgModule"));
        assert!(result.source.contains("\u{0275}\u{0275}defineInjector"));
        assert!(!result.source.contains("@NgModule"));

        let js = ngc_ts_transform::transform_source(&result.source, "app.module.ts");
        assert!(
            js.is_ok(),
            "oxc should parse compiled source: {:?}\n\nCompiled source:\n{}",
            js.err(),
            result.source
        );
    }

    #[test]
    fn test_plain_class_unchanged() {
        let source = "export class PlainClass { x = 1; }\n";
        let path = PathBuf::from("plain.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(!result.compiled);
        assert_eq!(result.source, source);
    }

    #[test]
    fn for_block_listener_resolves_loop_variable_via_ctx() {
        // Regression test for #40: a (click) handler inside @for was receiving
        // `undefined` for the loop variable because the listener preamble read
        // `_r.$implicit` (LView) instead of `_ctx.$implicit` (template context).
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-test',
  standalone: true,
  template: `
    @for (group of items; track group) {
      <button (click)="onClick(group)">{{ group }}</button>
    }
  `,
})
export class TestComponent {
  items: string[] = [];
  onClick(_group: string) {}
}
"#;
        let path = PathBuf::from("for-listener.component.ts");
        let result = compile_component(source, &path).expect("should compile");
        assert!(result.compiled);
        assert!(
            result.source.contains("const group = _ctx.$implicit"),
            "listener preamble must read the loop item from _ctx.$implicit; got:\n{}",
            result.source
        );
        assert!(
            !result.source.contains("const group = _r.$implicit"),
            "listener preamble must NOT read from _r.$implicit (LView has no $implicit); got:\n{}",
            result.source
        );

        let js = ngc_ts_transform::transform_source(&result.source, "for-listener.component.ts");
        assert!(
            js.is_ok(),
            "compiled source should be valid JS: {:?}",
            js.err()
        );
    }

    #[test]
    fn nested_for_iterable_does_not_prefix_outer_loop_variable_with_ctx() {
        // Regression: in @for (inner of outer.items), the iterable passed to
        // ɵɵrepeater() was being compiled with a `ctx.` prefix even though
        // `outer` is an outer @for's loop variable (a local), producing
        // `ɵɵrepeater(ctx.outer.items)` — which throws at runtime.
        let source = r#"import { Component } from '@angular/core';

interface Group { items: string[]; }

@Component({
  selector: 'app-test',
  standalone: true,
  template: `
    @for (outer of groups; track outer) {
      @for (inner of outer.items; track inner) {
        <span>{{ inner }}</span>
      }
    }
  `,
})
export class TestComponent {
  groups: Group[] = [];
}
"#;
        let path = PathBuf::from("nested-for-iterable.component.ts");
        let result = compile_component(source, &path).expect("should compile");
        assert!(result.compiled);
        assert!(
            result
                .source
                .contains("\u{0275}\u{0275}repeater(outer.items)"),
            "inner @for iterable must read outer.items without ctx. prefix; got:\n{}",
            result.source
        );
        assert!(
            !result
                .source
                .contains("\u{0275}\u{0275}repeater(ctx.outer.items)"),
            "inner @for iterable must NOT get ctx. prefix on outer loop variable; got:\n{}",
            result.source
        );

        let js =
            ngc_ts_transform::transform_source(&result.source, "nested-for-iterable.component.ts");
        assert!(
            js.is_ok(),
            "compiled source should be valid JS: {:?}",
            js.err()
        );
    }

    #[test]
    fn nested_for_block_listener_resolves_ancestor_loop_variables() {
        // Regression test for #40 (nested case): inside nested @for blocks,
        // listeners must also resolve *ancestor* loop variables via the
        // ɵɵnextContext chain, not just the innermost one.
        let source = r#"import { Component } from '@angular/core';

interface Group { items: string[]; }

@Component({
  selector: 'app-test',
  standalone: true,
  template: `
    @for (outer of groups; track outer) {
      @for (inner of outer.items; track inner) {
        <button (click)="onClick(outer, inner)">{{ inner }}</button>
      }
    }
  `,
})
export class TestComponent {
  groups: Group[] = [];
  onClick(_outer: Group, _inner: string) {}
}
"#;
        let path = PathBuf::from("nested-for-listener.component.ts");
        let result = compile_component(source, &path).expect("should compile");
        assert!(result.compiled);
        assert!(
            result.source.contains("const inner = _ctx.$implicit"),
            "innermost @for item must be read from _ctx.$implicit; got:\n{}",
            result.source
        );
        assert!(
            result
                .source
                .contains("const _outer_ctx = \u{0275}\u{0275}nextContext()")
                || result
                    .source
                    .contains("const _outer_ctx = \u{0275}\u{0275}nextContext(1)"),
            "ancestor @for must be reached via ɵɵnextContext; got:\n{}",
            result.source
        );
        assert!(
            result.source.contains("const outer = _outer_ctx.$implicit"),
            "ancestor @for item must be read from the ɵɵnextContext()'d context; got:\n{}",
            result.source
        );
        assert!(
            !result.source.contains("_r.$implicit"),
            "listener preamble must never dereference $implicit on _r (LView); got:\n{}",
            result.source
        );

        let js =
            ngc_ts_transform::transform_source(&result.source, "nested-for-listener.component.ts");
        assert!(
            js.is_ok(),
            "compiled source should be valid JS: {:?}",
            js.err()
        );
    }

    #[test]
    fn inline_svg_component_emits_namespace_transitions() {
        // End-to-end fixture covering issue #60: a realistic inline-SVG icon
        // with a foreignObject subtree must compile and emit ɵɵnamespaceSVG /
        // ɵɵnamespaceHTML transitions at the right positions so the browser
        // attaches the elements under the SVG namespace.
        let source = r#"import { Component } from '@angular/core';

@Component({
  selector: 'app-icon',
  standalone: true,
  template: `
    <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
      <g fill="currentColor">
        <path d="M12 2 L2 22 L22 22 Z"></path>
      </g>
      <foreignObject x="0" y="0" width="24" height="24">
        <div class="label">icon</div>
      </foreignObject>
    </svg>
    <span class="caption">Logo</span>
  `,
})
export class IconComponent {}
"#;
        let path = PathBuf::from("icon.component.ts");
        let result = compile_file(source, &path, &StyleContext::default()).expect("should compile");
        assert!(result.compiled, "should be compiled");

        let out = &result.source;
        assert!(
            out.contains("\u{0275}\u{0275}namespaceSVG"),
            "ɵɵnamespaceSVG must be emitted for inline SVG: {out}"
        );
        assert!(
            out.contains("\u{0275}\u{0275}namespaceHTML"),
            "ɵɵnamespaceHTML must be emitted (foreignObject descendants + trailing span): {out}"
        );

        let svg_ns = out.find("\u{0275}\u{0275}namespaceSVG()").unwrap();
        let svg_start = out
            .find("\u{0275}\u{0275}elementStart(0, 'svg'")
            .expect("svg elementStart with slot 0");
        assert!(
            svg_ns < svg_start,
            "ɵɵnamespaceSVG must precede elementStart('svg'): {out}"
        );

        // Rewritten component source must still parse through ts-transform
        // — the pipeline ngc-rs build runs before emitting JS.
        let js = ngc_ts_transform::transform_source(&result.source, "icon.component.ts")
            .expect("oxc should parse compiled source");
        assert!(js.contains("\u{0275}\u{0275}namespaceSVG"));
    }
}
