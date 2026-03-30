//! Node modules resolution for the ngc-rs bundler.
//!
//! Resolves bare import specifiers (e.g. `@angular/core`, `rxjs/operators`) to
//! their ESM entry points in `node_modules`, then recursively crawls all
//! transitive imports to discover every file that needs to be bundled.

pub mod package_json;
