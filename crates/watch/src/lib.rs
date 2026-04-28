//! File-system watcher with debounced incremental rebuilds for ngc-rs.
//!
//! Wraps the [`notify`] crate's recommended cross-platform watcher in a sync,
//! rayon-friendly API: callers register a build callback and a set of
//! subscribers, and the watcher coalesces rapid filesystem events into a
//! single rebuild trigger via a fixed debounce window.
//!
//! The watcher is intentionally generic over the build step — it does not
//! know anything about the ngc-rs pipeline — so the same crate can drive the
//! CLI's `watch` subcommand and (in a future milestone) the dev-server's
//! live-reload bridge.

mod debouncer;
mod event;
mod watcher;

pub use debouncer::{Debouncer, DebouncerConfig};
pub use event::{subscribers, NoopSubscriber, WatchEvent, WatchSubscriber};
pub use watcher::{Watcher, WatcherConfig};
