//! TypeScript-to-JavaScript transform for ngc-rs.
//!
//! This crate uses oxc to parse TypeScript source files, strip type annotations,
//! interfaces, type aliases, and decorators, then emit plain JavaScript.

mod transform;

pub use transform::{
    transform_project, transform_source, transform_sources_to_memory, transform_to_memory,
    TransformResult, TransformedModule,
};
