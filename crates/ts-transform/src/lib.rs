//! TypeScript-to-JavaScript transform for ngc-rs.
//!
//! This crate uses oxc to parse TypeScript source files, strip type annotations,
//! interfaces, type aliases, and decorators, then emit plain JavaScript.

mod defines;
mod transform;

pub use defines::{apply_defines, apply_defines_to_modules, DefineMap};
pub use transform::{
    transform_project, transform_source, transform_source_with_map, transform_sources_to_memory,
    transform_sources_to_memory_with_maps, transform_to_memory, transform_to_memory_with_maps,
    TransformResult, TransformedModule,
};
