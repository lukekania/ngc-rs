//! Minification pass for bundled JavaScript chunks.
//!
//! Re-emits each chunk through `oxc_codegen` with whitespace minification enabled,
//! optionally composing the resulting source map with the bundle source map.

use std::path::PathBuf;

use ngc_diagnostics::{NgcError, NgcResult};
use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_sourcemap::SourceMap;
use oxc_span::SourceType;

/// The result of minifying a single chunk.
pub struct MinifiedChunk {
    /// The minified JavaScript code.
    pub code: String,
    /// Source map from minified positions to original source positions.
    /// `None` when source map generation is disabled.
    pub source_map: Option<SourceMap>,
}

/// Minify a JavaScript chunk using oxc_codegen whitespace removal.
///
/// Parses the chunk code, re-emits it with `minify: true`, and optionally
/// composes the resulting source map with the bundle source map to produce
/// a map from minified positions directly to original TypeScript positions.
pub fn minify_chunk(
    code: &str,
    filename: &str,
    bundle_map: Option<&SourceMap>,
) -> NgcResult<MinifiedChunk> {
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();

    if parsed.panicked {
        return Err(NgcError::BundleError {
            message: format!("minification parse failed for {filename}"),
        });
    }

    let generate_map = bundle_map.is_some();
    let codegen_options = CodegenOptions {
        minify: true,
        source_map_path: if generate_map {
            Some(PathBuf::from(filename))
        } else {
            None
        },
        ..CodegenOptions::default()
    };

    let codegen_ret = Codegen::new()
        .with_options(codegen_options)
        .with_source_text(code)
        .build(&parsed.program);

    // Compose: minified->bundle + bundle->original = minified->original
    let final_map = match (codegen_ret.map, bundle_map) {
        (Some(minify_map), Some(bmap)) => Some(compose_source_maps(&minify_map, bmap)),
        _ => None,
    };

    Ok(MinifiedChunk {
        code: codegen_ret.code,
        source_map: final_map,
    })
}

/// Compose two source maps: `outer` maps A->B, `inner` maps B->C, result maps A->C.
///
/// For each token in `outer`, looks up the corresponding position in `inner`
/// using a lookup table, producing a resolved token that maps directly from
/// outer destination to inner source.
fn compose_source_maps(outer: &SourceMap, inner: &SourceMap) -> SourceMap {
    let lookup = inner.generate_lookup_table();

    let mut names: Vec<std::sync::Arc<str>> = Vec::new();
    let mut sources: Vec<std::sync::Arc<str>> = Vec::new();
    let mut source_contents: Vec<Option<std::sync::Arc<str>>> = Vec::new();
    let mut tokens: Vec<oxc_sourcemap::Token> = Vec::new();

    // Build mappings from inner's sources and names
    let inner_sources: Vec<_> = inner.get_sources().cloned().collect();
    let inner_source_contents: Vec<Option<std::sync::Arc<str>>> = inner
        .get_source_contents()
        .map(|opt| opt.cloned())
        .collect();
    let inner_names: Vec<_> = inner.get_names().cloned().collect();

    // Map from inner source/name IDs to our output IDs
    let mut source_id_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut name_id_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();

    for token in outer.get_tokens() {
        let src_line = token.get_src_line();
        let src_col = token.get_src_col();

        if let Some(resolved) = inner.lookup_token(&lookup, src_line, src_col) {
            let out_source_id = resolved.get_source_id().map(|sid| {
                *source_id_map.entry(sid).or_insert_with(|| {
                    let id = sources.len() as u32;
                    sources.push(inner_sources[sid as usize].clone());
                    if (sid as usize) < inner_source_contents.len() {
                        source_contents.push(inner_source_contents[sid as usize].clone());
                    } else {
                        source_contents.push(None);
                    }
                    id
                })
            });

            let out_name_id = resolved.get_name_id().map(|nid| {
                *name_id_map.entry(nid).or_insert_with(|| {
                    let id = names.len() as u32;
                    names.push(inner_names[nid as usize].clone());
                    id
                })
            });

            tokens.push(oxc_sourcemap::Token::new(
                token.get_dst_line(),
                token.get_dst_col(),
                resolved.get_src_line(),
                resolved.get_src_col(),
                out_source_id,
                out_name_id,
            ));
        }
    }

    SourceMap::new(
        None,
        names,
        None,
        sources,
        source_contents,
        tokens.into_boxed_slice(),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minify_reduces_size() {
        let code = "const   x   =   42;\nconst   y   =   'hello  world';\nconsole.log(x,   y);\n";
        let result = minify_chunk(code, "test.js", None).expect("should minify");
        assert!(
            result.code.len() < code.len(),
            "minified code should be shorter"
        );
        assert!(result.code.contains("42"), "should preserve values");
        assert!(result.source_map.is_none(), "no map without bundle map");
    }

    #[test]
    fn test_minify_with_source_map() {
        let code = "const x = 42;\nconsole.log(x);\n";

        // Create a simple identity bundle source map
        let bundle_map = SourceMap::new(
            None,
            vec![],
            None,
            vec!["original.ts".into()],
            vec![Some("const x: number = 42;\nconsole.log(x);\n".into())],
            vec![
                oxc_sourcemap::Token::new(0, 0, 0, 0, Some(0), None),
                oxc_sourcemap::Token::new(1, 0, 1, 0, Some(0), None),
            ]
            .into_boxed_slice(),
            None,
        );

        let result =
            minify_chunk(code, "test.js", Some(&bundle_map)).expect("should minify with map");
        assert!(result.source_map.is_some(), "should produce source map");
        let map = result.source_map.as_ref().expect("map exists");
        let sources: Vec<_> = map.get_sources().collect();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].as_ref(), "original.ts");
    }

    #[test]
    fn test_compose_source_maps() {
        // outer: minified -> bundle (line 0 col 0 -> line 1 col 0)
        let outer = SourceMap::new(
            None,
            vec![],
            None,
            vec!["bundle.js".into()],
            vec![],
            vec![oxc_sourcemap::Token::new(0, 0, 1, 0, Some(0), None)].into_boxed_slice(),
            None,
        );
        // inner: bundle -> original (line 1 col 0 -> line 5 col 3)
        let inner = SourceMap::new(
            None,
            vec![],
            None,
            vec!["original.ts".into()],
            vec![],
            vec![oxc_sourcemap::Token::new(1, 0, 5, 3, Some(0), None)].into_boxed_slice(),
            None,
        );

        let composed = compose_source_maps(&outer, &inner);
        let sources: Vec<_> = composed.get_sources().collect();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].as_ref(), "original.ts");

        let tokens: Vec<_> = composed.get_tokens().collect();
        assert_eq!(tokens.len(), 1);
        // Result: minified line 0, col 0 -> original line 5, col 3
        assert_eq!(tokens[0].get_dst_line(), 0);
        assert_eq!(tokens[0].get_dst_col(), 0);
        assert_eq!(tokens[0].get_src_line(), 5);
        assert_eq!(tokens[0].get_src_col(), 3);
    }
}
