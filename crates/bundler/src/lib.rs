//! ESM concatenation bundler for ngc-rs.
//!
//! Takes transformed JavaScript modules and a dependency graph, then produces
//! bundled ESM files with external imports hoisted and project-local imports
//! inlined. Supports code splitting via dynamic `import()` detection.

mod chunk;
mod concat;
mod minify;
pub mod npm_wrap;
mod rewrite;
mod shake;

pub use chunk::{build_chunk_graph, Chunk, ChunkGraph, ChunkKind};
pub use concat::{bundle, BundleInput, BundleOptions, BundleOutput};
pub use rewrite::{ExternalImport, RewrittenModule};
