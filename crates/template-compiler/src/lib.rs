//! Angular template compiler for ngc-rs.
//!
//! Compiles `@Component` templates to Angular Ivy instructions. Parses template
//! HTML with pest, generates Ivy codegen (ɵɵdefineComponent, template function),
//! and rewrites TypeScript source to replace the decorator with static Ivy metadata.

mod ast;
mod codegen;
mod extract;
mod parser;
mod rewrite;

use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use rayon::prelude::*;
use tracing::debug;

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
    })
}

/// Extract the template function from the IvyOutput's defineComponent code.
fn extract_template_fn_from_ivy(ivy: &codegen::IvyOutput, class_name: &str) -> String {
    let dc = &ivy.define_component_code;
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
    let dc = &ivy.define_component_code;

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

/// Compile Angular component templates in the given TypeScript source files.
///
/// For each file that contains an `@Component` decorator with an inline `template`,
/// parses the template, generates Ivy instructions, and rewrites the source to
/// replace the decorator with static Ivy metadata. Files without `@Component`
/// decorators are returned unchanged.
///
/// Files are processed in parallel using rayon.
pub fn compile_templates(files: &[PathBuf]) -> NgcResult<Vec<CompiledFile>> {
    let results: Vec<NgcResult<CompiledFile>> = files
        .par_iter()
        .map(|file_path| {
            let source = std::fs::read_to_string(file_path).map_err(|e| NgcError::Io {
                path: file_path.clone(),
                source: e,
            })?;

            compile_component(&source, file_path)
        })
        .collect();

    results.into_iter().collect()
}

/// Compile a single TypeScript source string containing an Angular component.
///
/// If the source contains an `@Component` decorator with an inline `template`,
/// parses the template, generates Ivy instructions, and rewrites the source.
/// If no `@Component` is found, returns the source unchanged.
pub fn compile_component(source: &str, file_path: &Path) -> NgcResult<CompiledFile> {
    let extracted = match extract::extract_component(source, file_path)? {
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
    fn test_compile_and_transform_roundtrip() {
        // Read a real component file if available, otherwise use inline source
        let path = PathBuf::from("/Users/lukaskania/Coding/Private/treasr/treasr-frontend/src/app/components/skeleton-loader.component.ts");
        let source = if path.exists() {
            std::fs::read_to_string(&path).unwrap()
        } else {
            "import { Component, Input } from '@angular/core';\n\n@Component({\n  selector: 'app-test',\n  standalone: true,\n  template: '<div [class]=\"cls\"><span>Hi</span></div>',\n})\nexport class TestComponent {\n  @Input() cls = '';\n}\n".to_string()
        };
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
}
