use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
use rayon::prelude::*;
use tracing::debug;

/// Result of transforming an entire project.
#[derive(Debug)]
pub struct TransformResult {
    /// Number of files successfully transformed.
    pub files_transformed: usize,
    /// Output directory where JS files were written.
    pub out_dir: PathBuf,
}

/// A single transformed module held in memory.
#[derive(Debug, Clone)]
pub struct TransformedModule {
    /// The original canonical source path (matches the graph node).
    pub source_path: PathBuf,
    /// The generated JavaScript code.
    pub code: String,
    /// Source map from original TypeScript to generated JavaScript.
    /// `None` when source map generation is disabled.
    pub source_map: Option<oxc_sourcemap::SourceMap>,
}

/// Transform a single TypeScript source string into JavaScript.
///
/// Parses the input as TypeScript, strips type annotations, interfaces,
/// type aliases, and decorators, then returns the generated JavaScript.
pub fn transform_source(source: &str, file_name: &str) -> NgcResult<String> {
    let (code, _map) = transform_source_impl(source, file_name, false)?;
    Ok(code)
}

/// Transform a single TypeScript source string into JavaScript with an optional source map.
///
/// When `generate_source_map` is true, returns a source map mapping the generated
/// JavaScript back to the original TypeScript source.
pub fn transform_source_with_map(
    source: &str,
    file_name: &str,
    generate_source_map: bool,
) -> NgcResult<(String, Option<oxc_sourcemap::SourceMap>)> {
    transform_source_impl(source, file_name, generate_source_map)
}

/// Internal implementation shared by `transform_source` and `transform_source_with_map`.
fn transform_source_impl(
    source: &str,
    file_name: &str,
    generate_source_map: bool,
) -> NgcResult<(String, Option<oxc_sourcemap::SourceMap>)> {
    let allocator = Allocator::new();
    let path = Path::new(file_name);

    let source_type = SourceType::from_path(path).map_err(|_| NgcError::ParseError {
        path: path.to_path_buf(),
        message: format!("unsupported file extension: {file_name}"),
    })?;

    let mut parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked {
        return Err(NgcError::ParseError {
            path: path.to_path_buf(),
            message: "parser panicked".to_string(),
        });
    }

    if !parsed.errors.is_empty() {
        let messages: Vec<String> = parsed.errors.iter().map(|e| e.to_string()).collect();
        return Err(NgcError::ParseError {
            path: path.to_path_buf(),
            message: messages.join("; "),
        });
    }

    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;

    let mut options = TransformOptions::default();
    options.decorator.legacy = true;
    options.decorator.emit_decorator_metadata = false;

    let transformer = Transformer::new(&allocator, path, &options);
    let transform_ret =
        transformer.build_with_scoping(semantic.into_scoping(), &mut parsed.program);

    if !transform_ret.errors.is_empty() {
        let messages: Vec<String> = transform_ret.errors.iter().map(|e| e.to_string()).collect();
        return Err(NgcError::TransformError {
            path: path.to_path_buf(),
            message: messages.join("; "),
        });
    }

    let codegen_options = CodegenOptions {
        single_quote: true,
        source_map_path: if generate_source_map {
            Some(PathBuf::from(file_name))
        } else {
            None
        },
        ..CodegenOptions::default()
    };

    let codegen_ret = Codegen::new()
        .with_options(codegen_options)
        .with_source_text(source)
        .with_scoping(Some(transform_ret.scoping))
        .build(&parsed.program);

    Ok((codegen_ret.code, codegen_ret.map))
}

/// Transform all TypeScript files and write JavaScript output to `out_dir`.
///
/// Each input file is read, parsed, transformed (stripping types and decorators),
/// and written to the corresponding location under `out_dir`. Directory structure
/// relative to `root_dir` is preserved, and `.ts`/`.tsx` extensions are changed
/// to `.js`/`.jsx`.
///
/// Files are processed in parallel using rayon.
pub fn transform_project(
    files: &[PathBuf],
    root_dir: &Path,
    out_dir: &Path,
) -> NgcResult<TransformResult> {
    std::fs::create_dir_all(out_dir).map_err(|e| NgcError::Io {
        path: out_dir.to_path_buf(),
        source: e,
    })?;

    let results: Vec<NgcResult<PathBuf>> = files
        .par_iter()
        .map(|file_path| {
            let source = std::fs::read_to_string(file_path).map_err(|e| NgcError::Io {
                path: file_path.clone(),
                source: e,
            })?;

            let file_name = file_path.to_string_lossy();
            let js_code = transform_source(&source, &file_name)?;

            let relative = file_path.strip_prefix(root_dir).unwrap_or(file_path);
            let out_path = out_dir.join(relative);
            let out_path = map_extension(&out_path);

            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| NgcError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }

            std::fs::write(&out_path, js_code).map_err(|e| NgcError::Io {
                path: out_path.clone(),
                source: e,
            })?;

            debug!(?file_path, ?out_path, "transformed");
            Ok(out_path)
        })
        .collect();

    let mut count = 0;
    for result in results {
        result?;
        count += 1;
    }

    Ok(TransformResult {
        files_transformed: count,
        out_dir: out_dir.to_path_buf(),
    })
}

/// Transform all TypeScript files and return JavaScript in memory.
///
/// Each input file is read, parsed, and transformed (stripping types and
/// decorators). Results are returned as in-memory `TransformedModule` values
/// instead of being written to disk. Files are processed in parallel using rayon.
pub fn transform_to_memory(files: &[PathBuf]) -> NgcResult<Vec<TransformedModule>> {
    transform_to_memory_with_maps(files, false)
}

/// Transform all TypeScript files and return JavaScript in memory, optionally with source maps.
///
/// When `generate_source_maps` is true, each `TransformedModule` includes a source
/// map from the original TypeScript to the generated JavaScript.
pub fn transform_to_memory_with_maps(
    files: &[PathBuf],
    generate_source_maps: bool,
) -> NgcResult<Vec<TransformedModule>> {
    let results: Vec<NgcResult<TransformedModule>> = files
        .par_iter()
        .map(|file_path| {
            let source = std::fs::read_to_string(file_path).map_err(|e| NgcError::Io {
                path: file_path.clone(),
                source: e,
            })?;

            let file_name = file_path.to_string_lossy();
            let (code, source_map) =
                transform_source_with_map(&source, &file_name, generate_source_maps)?;

            debug!(?file_path, "transformed to memory");
            Ok(TransformedModule {
                source_path: file_path.clone(),
                code,
                source_map,
            })
        })
        .collect();

    results.into_iter().collect()
}

/// Transform pre-compiled TypeScript sources to JavaScript in memory.
///
/// Unlike `transform_to_memory`, this takes source strings directly rather than
/// reading from disk. Each tuple maps a canonical file path to its (potentially
/// template-compiled) TypeScript source. Files are processed in parallel using rayon.
pub fn transform_sources_to_memory(
    sources: &[(PathBuf, String)],
) -> NgcResult<Vec<TransformedModule>> {
    transform_sources_to_memory_with_maps(sources, false)
}

/// Transform pre-compiled TypeScript sources to JavaScript in memory, optionally with source maps.
///
/// When `generate_source_maps` is true, each `TransformedModule` includes a source
/// map from the original TypeScript to the generated JavaScript.
pub fn transform_sources_to_memory_with_maps(
    sources: &[(PathBuf, String)],
    generate_source_maps: bool,
) -> NgcResult<Vec<TransformedModule>> {
    let results: Vec<NgcResult<TransformedModule>> = sources
        .par_iter()
        .map(|(file_path, source)| {
            let file_name = file_path.to_string_lossy();
            let (code, source_map) =
                transform_source_with_map(source, &file_name, generate_source_maps)?;

            debug!(?file_path, "transformed source to memory");
            Ok(TransformedModule {
                source_path: file_path.clone(),
                code,
                source_map,
            })
        })
        .collect();

    results.into_iter().collect()
}

/// Map TypeScript extensions to JavaScript equivalents.
fn map_extension(path: &Path) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some("ts") => path.with_extension("js"),
        Some("tsx") => path.with_extension("jsx"),
        Some("mts") => path.with_extension("mjs"),
        Some("cts") => path.with_extension("cjs"),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_type_annotation() {
        let source = "const x: number = 42;\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(
            !result.contains(": number"),
            "type annotation should be stripped"
        );
        assert!(result.contains("const x = 42"), "value should be preserved");
    }

    #[test]
    fn test_strip_interface() {
        let source = "interface Foo { bar: string; }\nexport const x = 1;\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(
            !result.contains("interface"),
            "interface should be stripped"
        );
        assert!(
            result.contains("export const x = 1"),
            "value should be preserved"
        );
    }

    #[test]
    fn test_strip_type_alias() {
        let source = "type ID = string | number;\nexport const x = 1;\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(!result.contains("type ID"), "type alias should be stripped");
    }

    #[test]
    fn test_strip_enum() {
        let source = "export enum Direction { Up, Down, Left, Right }\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(!result.contains("enum "), "enum keyword should be stripped");
        assert!(
            result.contains("Direction"),
            "enum name should appear in output"
        );
    }

    #[test]
    fn test_preserve_value_import() {
        let source = "import { Component } from '@angular/core';\nComponent;\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(
            result.contains("@angular/core"),
            "value import should be preserved"
        );
    }

    #[test]
    fn test_strip_type_import() {
        let source = "import type { Routes } from '@angular/router';\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(
            !result.contains("@angular/router"),
            "type-only import should be stripped"
        );
    }

    #[test]
    fn test_parse_error() {
        let source = "const x: = ;; {{{{";
        let result = transform_source(source, "test.ts");
        assert!(result.is_err(), "invalid syntax should return error");
    }

    #[test]
    fn test_preserve_class() {
        let source = "export class Foo {\n  bar = 1;\n}\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(result.contains("class Foo"), "class should be preserved");
        assert!(result.contains("bar = 1"), "class body should be preserved");
    }

    #[test]
    fn test_localize_tagged_template_passthrough() {
        let source = "export const msg = $localize`:@@intro|greeting:Hello, ${name}:USER:!`;\n";
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(
            result.contains("$localize"),
            "$localize identifier should be preserved: {result}"
        );
        assert!(
            result.contains(":@@intro|greeting:"),
            "metadata block should be preserved verbatim: {result}"
        );
        assert!(
            result.contains(":USER:"),
            "placeholder name should be preserved: {result}"
        );
        assert!(
            result.contains("Hello, "),
            "static text should be preserved: {result}"
        );
    }

    #[test]
    fn test_strip_decorator() {
        let source = r#"function Component(config: any) { return (target: any) => target; }

@Component({
  selector: 'app-root',
  template: '<h1>Hello</h1>'
})
export class AppComponent {
  title = 'app';
}
"#;
        let result = transform_source(source, "test.ts").expect("should transform");
        assert!(
            !result.contains("@Component"),
            "decorator should be stripped"
        );
        assert!(
            result.contains("class AppComponent"),
            "class should be preserved"
        );
    }

    #[test]
    fn test_source_map_generation() {
        let source = "export const x: number = 42;\nexport function greet(name: string): string { return name; }\n";
        let (code, map) =
            transform_source_with_map(source, "test.ts", true).expect("should transform");
        assert!(!code.contains(": number"));
        let map = map.expect("source map should be present");
        // Source map should reference our file
        let sources: Vec<_> = map.get_sources().collect();
        assert!(!sources.is_empty(), "should have at least one source");
        // Should have tokens mapping output positions to input positions
        assert!(map.get_tokens().count() > 0, "should have mapping tokens");
    }

    #[test]
    fn test_source_map_disabled() {
        let source = "export const x = 42;\n";
        let (_code, map) =
            transform_source_with_map(source, "test.ts", false).expect("should transform");
        assert!(map.is_none(), "source map should be None when disabled");
    }

    #[test]
    fn test_map_extension_ts() {
        assert_eq!(map_extension(Path::new("foo.ts")), PathBuf::from("foo.js"));
    }

    #[test]
    fn test_map_extension_tsx() {
        assert_eq!(
            map_extension(Path::new("foo.tsx")),
            PathBuf::from("foo.jsx")
        );
    }

    #[test]
    fn test_map_extension_mts() {
        assert_eq!(
            map_extension(Path::new("foo.mts")),
            PathBuf::from("foo.mjs")
        );
    }

    #[test]
    fn test_map_extension_js_passthrough() {
        assert_eq!(map_extension(Path::new("foo.js")), PathBuf::from("foo.js"));
    }

    #[test]
    fn test_inline_scss_styles_template_literal_passes_through() {
        // Regression guard for GH #81. A component file whose `styles` array
        // contains a backtick template literal with SCSS `@use 'sass:color';`
        // pragmas and keyword-argument function calls (`$lightness: 60%`)
        // must parse cleanly as TypeScript — the SCSS text is opaque string
        // content, not JS syntax. Transform must not panic and must not
        // surface a parse error back to the caller.
        let source = "import { Component } from '@angular/core';\n\
\n\
@Component({\n\
  selector: 'app-scss-styles',\n\
  styleUrl: './scss-styles.component.scss',\n\
  styles: [`\n\
    @use 'sass:color';\n\
\n\
    $inline-color: #2e7d32;\n\
\n\
    .scss-inline {\n\
      background: color.adjust($inline-color, $lightness: 60%);\n\
    }\n\
  `],\n\
  template: `<p>hi</p>`,\n\
})\n\
export class ScssStylesComponent {}\n";
        let result = transform_source(source, "scss-styles.component.ts")
            .expect("inline SCSS styles should survive TS transform");
        assert!(
            result.contains("class ScssStylesComponent"),
            "class should be preserved: {result}"
        );
        assert!(
            !result.contains("@Component("),
            "decorator should be stripped: {result}"
        );
    }
}
