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
