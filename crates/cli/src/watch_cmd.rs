//! Glue between the `ngc-rs watch` subcommand and the `ngc-watch` crate.
//!
//! Owns the [`incremental::BuildCache`] for the lifetime of the watch loop
//! and translates filesystem events into invalidations + a fresh call to
//! `run_build_with_cache`. Subscribers attached via [`run`] receive every
//! [`ngc_watch::WatchEvent`] for downstream consumers (the dev-server in
//! issue #25, the builder adapter in #26).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use colored::Colorize;
use ngc_diagnostics::NgcResult;
use ngc_watch::{Watcher, WatcherConfig};

use crate::incremental::BuildCache;

/// Run the watch loop until `should_stop` returns `true`.
///
/// Performs an initial full build (populating the cache), then enters the
/// watcher loop. The build callback consults the cache so each incremental
/// rebuild only re-runs template compilation and ts-transform on files
/// whose bytes changed since the last build.
pub fn run(
    project: &Path,
    out_dir_override: Option<&Path>,
    configuration: Option<&str>,
    localize: bool,
    subscribers: Vec<Arc<dyn ngc_watch::WatchSubscriber>>,
    should_stop: impl FnMut(usize) -> bool,
) -> NgcResult<()> {
    let mut cache = BuildCache::new();

    // Initial build to populate the cache.
    let initial = crate::run_build_with_cache(
        project,
        out_dir_override,
        configuration,
        localize,
        Some(&mut cache),
    )?;
    eprintln!(
        "{} {} module(s), {} file(s) → {}",
        "ngc-rs watch ready".bold().green(),
        initial.modules_bundled,
        initial.output_files.len(),
        format_size(initial.total_size_bytes),
    );

    // Determine the directory to watch. Falls back to the project file's
    // parent directory.
    let root = watch_root(project);
    let cfg = WatcherConfig::new(root);
    let mut watcher = Watcher::new(cfg);
    for s in subscribers {
        watcher.subscribe(s);
    }

    let project_path = project.to_path_buf();
    let out_dir_path = out_dir_override.map(|p| p.to_path_buf());
    let configuration = configuration.map(|s| s.to_string());

    let build_fn = move |dirty: &[PathBuf]| -> NgcResult<()> {
        // Coarse invalidation: any non-`.ts` change might affect any file's
        // template/style compilation, so we wipe the whole cache. For
        // `.ts`-only changes we drop just the affected entries.
        if dirty.iter().any(|p| !is_ts_path(p)) {
            cache.clear();
        } else {
            cache.invalidate(dirty);
        }
        let result = crate::run_build_with_cache(
            &project_path,
            out_dir_path.as_deref(),
            configuration.as_deref(),
            localize,
            Some(&mut cache),
        )?;
        eprintln!(
            "{} {} module(s), {} dirty",
            "ngc-rs rebuild".bold().green(),
            result.modules_bundled,
            dirty.len()
        );
        Ok(())
    };

    watcher.run_until(build_fn, should_stop)
}

/// Pick the directory the watcher should monitor for changes.
///
/// Defaults to the parent directory of `project` (the tsconfig file). Falls
/// back to the current working directory when `project` is a bare filename
/// or has no parent component.
pub(crate) fn watch_root(project: &Path) -> PathBuf {
    project
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `true` when `p` has a `.ts`/`.tsx` extension. Used to decide whether a
/// dirty-set is small enough for surgical cache invalidation versus a full
/// flush (templates and styles cross-cut the cache).
pub(crate) fn is_ts_path(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("ts") | Some("tsx")
    )
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_root_resolves_to_parent_dir() {
        assert_eq!(
            watch_root(Path::new("/proj/tsconfig.json")),
            Path::new("/proj")
        );
    }

    #[test]
    fn watch_root_falls_back_to_dot_for_bare_filename() {
        assert_eq!(watch_root(Path::new("tsconfig.json")), PathBuf::from("."));
    }

    #[test]
    fn is_ts_path_recognizes_ts_and_tsx() {
        assert!(is_ts_path(Path::new("a.ts")));
        assert!(is_ts_path(Path::new("a.tsx")));
        assert!(!is_ts_path(Path::new("a.html")));
        assert!(!is_ts_path(Path::new("a.css")));
        assert!(!is_ts_path(Path::new("a")));
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0 MB");
    }
}
