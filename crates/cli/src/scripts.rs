//! Global `scripts` array support — emit one bundle per `bundleName`
//! by concatenating each entry's contents and writing a single non-module
//! JS file to `out_dir`.
//!
//! Mirrors the semantics of `@angular/build:application`'s `scripts`
//! option: entries are loaded as plain (non-ES-module) JS in the order
//! declared, before the application module. Multiple entries that share
//! a `bundleName` are concatenated into one file; entries with
//! `inject: false` are written to disk but not referenced from
//! `index.html`.
//!
//! These bundles are intentionally not run through the JS bundler — the
//! files are typically third-party CDN snippets (analytics, polyfill
//! shims) that aren't ES modules and would break under module-graph
//! resolution. Concatenation matches what `@angular/build` does once
//! its esbuild pass collapses the IIFE wrappers.

use std::path::{Path, PathBuf};

use ngc_bundler::BundleOptions;
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_project_resolver::angular_json::ResolvedScriptBundle;
use sha2::{Digest, Sha256};

/// One emitted script bundle: the on-disk filename (with hash applied
/// when [`BundleOptions::content_hash`] is set) and whether it should
/// be referenced from `index.html`.
#[derive(Debug, Clone)]
pub(crate) struct EmittedScriptBundle {
    /// Final filename written to `out_dir`, e.g. `scripts.a1b2c3d4.js`
    /// in production or `scripts.js` otherwise.
    pub filename: String,
    /// Whether to emit a `<script defer src="…">` tag for this bundle
    /// in the generated `index.html`.
    pub inject: bool,
}

/// Concatenate each [`ResolvedScriptBundle`]'s sources into one JS file
/// and write it to `out_dir`. Returns one [`EmittedScriptBundle`] per
/// bundle (in the order resolved from `angular.json`) plus the absolute
/// paths of every file written so the caller can fold them into the
/// build's `output_files` list.
pub(crate) fn emit_script_bundles(
    bundles: &[ResolvedScriptBundle],
    out_dir: &Path,
    bundle_options: BundleOptions,
) -> NgcResult<(Vec<EmittedScriptBundle>, Vec<PathBuf>)> {
    let mut emitted = Vec::with_capacity(bundles.len());
    let mut written_paths = Vec::with_capacity(bundles.len());
    for bundle in bundles {
        let mut concatenated = String::new();
        for source in &bundle.sources {
            let body = std::fs::read_to_string(source).map_err(|e| NgcError::Io {
                path: source.clone(),
                source: e,
            })?;
            concatenated.push_str(&format!("/* {} */\n", source.display()));
            concatenated.push_str(&body);
            if !body.ends_with('\n') {
                concatenated.push('\n');
            }
        }
        let filename = if bundle_options.content_hash {
            let hash = content_hash(&concatenated);
            format!("{}.{hash}.js", bundle.name)
        } else {
            format!("{}.js", bundle.name)
        };
        let path = out_dir.join(&filename);
        std::fs::write(&path, &concatenated).map_err(|e| NgcError::Io {
            path: path.clone(),
            source: e,
        })?;
        written_paths.push(path);
        emitted.push(EmittedScriptBundle {
            filename,
            inject: bundle.inject,
        });
    }
    Ok((emitted, written_paths))
}

/// SHA-256 over the concatenated bytes, hex-encoded, truncated to 8
/// chars — matches the hash length used by the polyfills + main chunks.
fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let bytes = hasher.finalize();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    #[test]
    fn single_bundle_default_name_no_hash() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = write_temp_script(dir.path(), "global.js", "window.x = 1;");
        let bundles = vec![ResolvedScriptBundle {
            name: "scripts".into(),
            sources: vec![src],
            inject: true,
        }];
        let (emitted, paths) =
            emit_script_bundles(&bundles, dir.path(), BundleOptions::default()).unwrap();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].filename, "scripts.js");
        assert!(emitted[0].inject);
        assert_eq!(paths.len(), 1);
        let written = std::fs::read_to_string(dir.path().join("scripts.js")).unwrap();
        assert!(written.contains("window.x = 1;"));
        // Ensure trailing newline is added for files that lack one.
        assert!(written.ends_with('\n'));
    }

    #[test]
    fn multi_entry_bundle_concatenates_in_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = write_temp_script(dir.path(), "a.js", "var a = 1;\n");
        let b = write_temp_script(dir.path(), "b.js", "var b = 2;\n");
        let bundles = vec![ResolvedScriptBundle {
            name: "scripts".into(),
            sources: vec![a, b],
            inject: true,
        }];
        let (_, _) = emit_script_bundles(&bundles, dir.path(), BundleOptions::default()).unwrap();
        let written = std::fs::read_to_string(dir.path().join("scripts.js")).unwrap();
        let a_pos = written.find("var a = 1;").expect("a present");
        let b_pos = written.find("var b = 2;").expect("b present");
        assert!(a_pos < b_pos, "sources should be concatenated in order");
    }

    #[test]
    fn content_hash_applied_in_production() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = write_temp_script(dir.path(), "global.js", "window.x = 1;\n");
        let bundles = vec![ResolvedScriptBundle {
            name: "scripts".into(),
            sources: vec![src],
            inject: true,
        }];
        let opts = BundleOptions {
            content_hash: true,
            ..BundleOptions::default()
        };
        let (emitted, _) = emit_script_bundles(&bundles, dir.path(), opts).unwrap();
        assert_eq!(emitted.len(), 1);
        let filename = &emitted[0].filename;
        assert!(filename.starts_with("scripts."));
        assert!(filename.ends_with(".js"));
        // Hash is exactly 8 hex chars.
        let hash_part = filename
            .strip_prefix("scripts.")
            .and_then(|s| s.strip_suffix(".js"))
            .unwrap();
        assert_eq!(hash_part.len(), 8);
        assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn inject_false_propagates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = write_temp_script(dir.path(), "lazy.js", "noop();\n");
        let bundles = vec![ResolvedScriptBundle {
            name: "lazy".into(),
            sources: vec![src],
            inject: false,
        }];
        let (emitted, _) =
            emit_script_bundles(&bundles, dir.path(), BundleOptions::default()).unwrap();
        assert_eq!(emitted[0].filename, "lazy.js");
        assert!(!emitted[0].inject);
    }
}
