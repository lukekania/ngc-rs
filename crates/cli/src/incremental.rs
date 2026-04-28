//! Per-module cache used by the `watch` subcommand to avoid redoing
//! template compilation and TypeScript transformation for files whose
//! on-disk content is unchanged since the last successful build.
//!
//! The cache is keyed by canonical source path; each entry stores the
//! source bytes' SHA-256 plus the corresponding template-compiler and
//! ts-transform outputs. On rebuild, we hash each source file once and
//! consult the cache before doing real work — a hit means we reuse the
//! cached AOT output and the cached transformed JS verbatim.
//!
//! Hashing the bytes (rather than relying on mtime) is what guarantees
//! the "unchanged chunks keep their content-hash filename" property: a
//! rebuild caused by an unrelated file will reuse byte-identical
//! transformed output for every untouched module, so the bundler emits
//! byte-identical chunks for those modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_sourcemap::SourceMap;
use sha2::{Digest, Sha256};

/// Hash of a source file's bytes. We use SHA-256 rather than a faster
/// non-cryptographic hash because cache misses are cheap (re-run the
/// transform) but cache *false positives* would silently bake stale code
/// into the build — collision resistance buys correctness.
pub type SourceHash = [u8; 32];

/// One file's cached pipeline outputs.
#[derive(Debug, Clone)]
pub struct CachedModule {
    /// Hash of the on-disk source bytes when this entry was populated.
    pub source_hash: SourceHash,
    /// Output of [`ngc_template_compiler::compile_file_with_styles`] —
    /// the post-AOT TS source.
    pub compiled_source: String,
    /// Whether this file used a JIT fallback during the original compile.
    pub jit_fallback: bool,
    /// Post-transform JavaScript.
    pub transformed_code: String,
    /// Optional source map for the transformed JS.
    pub transformed_map: Option<SourceMap>,
}

/// Per-build-pipeline module cache.
#[derive(Debug, Default)]
pub struct BuildCache {
    entries: HashMap<PathBuf, CachedModule>,
}

impl BuildCache {
    /// Empty cache; the first build populates it, subsequent builds
    /// consult and refresh it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hash a file's bytes. Returns `None` if the file cannot be read.
    pub fn hash_file(path: &Path) -> Option<SourceHash> {
        let bytes = std::fs::read(path).ok()?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Some(hasher.finalize().into())
    }

    /// Hash a source string in memory (for callers that already have the
    /// bytes — keeps cache lookups consistent with `hash_file`).
    pub fn hash_bytes(bytes: &[u8]) -> SourceHash {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    }

    /// Look up an entry by path; returns `Some` only when the cached
    /// hash matches the supplied `current_hash`. Mismatches invalidate
    /// the entry and return `None`.
    pub fn get_fresh(&self, path: &Path, current_hash: &SourceHash) -> Option<&CachedModule> {
        let entry = self.entries.get(path)?;
        if &entry.source_hash == current_hash {
            Some(entry)
        } else {
            None
        }
    }

    /// Insert or replace the entry for `path`.
    pub fn insert(&mut self, path: PathBuf, module: CachedModule) {
        self.entries.insert(path, module);
    }

    /// Mutable accessor for an existing entry (used by the transform step
    /// to fill in `transformed_code` after the template-compile step has
    /// already populated the row).
    pub fn entries_get_mut(&mut self, path: &Path) -> Option<&mut CachedModule> {
        self.entries.get_mut(path)
    }

    /// Drop entries for the supplied paths. Used when the watcher reports
    /// changes — we forget the prior outputs so the rebuild always
    /// recomputes them, even if (e.g. on a file rename) the new bytes
    /// happen to hash to the previous value.
    pub fn invalidate(&mut self, paths: &[PathBuf]) {
        for p in paths {
            self.entries.remove(p);
        }
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached entries (one per source file with a hit).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the cache has no entries.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entry(hash: SourceHash) -> CachedModule {
        CachedModule {
            source_hash: hash,
            compiled_source: "// compiled".to_string(),
            jit_fallback: false,
            transformed_code: "// transformed".to_string(),
            transformed_map: None,
        }
    }

    #[test]
    fn new_cache_is_empty() {
        let c = BuildCache::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn insert_and_lookup() {
        let mut c = BuildCache::new();
        let h = BuildCache::hash_bytes(b"hello");
        c.insert(PathBuf::from("a.ts"), entry(h));
        assert_eq!(c.len(), 1);
        assert!(c.get_fresh(Path::new("a.ts"), &h).is_some());
    }

    #[test]
    fn lookup_returns_none_on_hash_mismatch() {
        let mut c = BuildCache::new();
        let h = BuildCache::hash_bytes(b"hello");
        c.insert(PathBuf::from("a.ts"), entry(h));
        let other = BuildCache::hash_bytes(b"world");
        assert!(c.get_fresh(Path::new("a.ts"), &other).is_none());
    }

    #[test]
    fn invalidate_removes_paths() {
        let mut c = BuildCache::new();
        let h = BuildCache::hash_bytes(b"x");
        c.insert(PathBuf::from("a.ts"), entry(h));
        c.insert(PathBuf::from("b.ts"), entry(h));
        c.invalidate(&[PathBuf::from("a.ts")]);
        assert_eq!(c.len(), 1);
        assert!(c.get_fresh(Path::new("a.ts"), &h).is_none());
        assert!(c.get_fresh(Path::new("b.ts"), &h).is_some());
    }

    #[test]
    fn clear_drops_all() {
        let mut c = BuildCache::new();
        let h = BuildCache::hash_bytes(b"x");
        c.insert(PathBuf::from("a.ts"), entry(h));
        c.clear();
        assert!(c.is_empty());
    }

    #[test]
    fn hash_file_reads_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("seed.ts");
        let mut f = std::fs::File::create(&path).expect("create seed");
        write!(f, "console.log('hi');").expect("write seed");
        let h = BuildCache::hash_file(&path).expect("hash seed");
        let expected = BuildCache::hash_bytes(b"console.log('hi');");
        assert_eq!(h, expected);
    }

    #[test]
    fn hash_file_returns_none_for_missing_path() {
        let h = BuildCache::hash_file(Path::new("/no/such/file/here-abc.ts"));
        assert!(h.is_none());
    }
}
