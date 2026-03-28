//! ESM concatenation bundler for ngc-rs.
//!
//! Takes transformed JavaScript modules and a dependency graph, then produces
//! a single bundled ESM file with external imports hoisted and project-local
//! imports inlined.

mod concat;
mod rewrite;

pub use concat::{bundle, BundleInput};
pub use rewrite::{ExternalImport, RewrittenModule};
