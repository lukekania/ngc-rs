//! Glue between the `ngc-rs serve` subcommand, the file watcher (#24), and
//! the HTTP dev server (#25).
//!
//! The flow is:
//!
//! 1. Resolve the project's effective `dist/` directory.
//! 2. Run a full build to populate it (and seed the incremental cache).
//! 3. Start [`ngc_dev_server::DevServer`] against that directory.
//! 4. Drive [`ngc_watch::Watcher`] on the project root; every successful
//!    rebuild forwards a [`ReloadEvent`] to the dev server, which fans it
//!    out to connected browsers via SSE.
//! 5. Install a Ctrl+C handler so the watcher loop exits cleanly and the
//!    dev server is dropped before the process ends.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;

use std::collections::HashMap;
use std::sync::Mutex;

use colored::Colorize;
use ngc_dev_server::{
    ComponentUpdates, DevServer, DevServerConfig, DevServerEvent, TlsConfig, HMR_RUNTIME_PRELUDE,
};
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_watch::{Watcher, WatcherConfig};

use crate::incremental::BuildCache;
use crate::watch_cmd::{is_ts_path, watch_root};

/// Run the `serve` subcommand: bring up the dev server, drive the watcher,
/// and block until Ctrl+C.
#[allow(clippy::too_many_arguments)]
pub fn run(
    project: &Path,
    configuration: Option<&str>,
    host: &str,
    port: u16,
    open: bool,
    serve_path: Option<&str>,
    allowed_hosts: &[String],
    headers: &[(String, String)],
    ssl: bool,
    ssl_key: Option<&Path>,
    ssl_cert: Option<&Path>,
    hmr_override: Option<bool>,
) -> NgcResult<()> {
    run_with_stop(
        project,
        configuration,
        host,
        port,
        open,
        serve_path,
        allowed_hosts,
        headers,
        ssl,
        ssl_key,
        ssl_cert,
        hmr_override,
        install_ctrlc,
    )
}

/// Resolve the `--ssl`/`--ssl-key`/`--ssl-cert` flags into an optional
/// [`TlsConfig`].
///
/// * `ssl` off → `None` (plain HTTP), and supplying a key/cert path without
///   `--ssl` is rejected so a typo doesn't silently serve over HTTP.
/// * `ssl` on with both a key and cert path → load that PEM material.
/// * `ssl` on with only one of the two → an error, since both halves are
///   required.
/// * `ssl` on with neither → mint a throwaway self-signed certificate for
///   the bind host (matching `@angular/build:dev-server`).
fn resolve_tls(
    ssl: bool,
    ssl_key: Option<&Path>,
    ssl_cert: Option<&Path>,
    host: &str,
) -> NgcResult<Option<TlsConfig>> {
    if !ssl {
        if ssl_key.is_some() || ssl_cert.is_some() {
            return Err(NgcError::ServeError {
                message: "--ssl-key/--ssl-cert require --ssl to be set".to_string(),
            });
        }
        return Ok(None);
    }

    match (ssl_key, ssl_cert) {
        (Some(key_path), Some(cert_path)) => {
            let key_pem = std::fs::read(key_path).map_err(|source| NgcError::Io {
                path: key_path.to_path_buf(),
                source,
            })?;
            let cert_pem = std::fs::read(cert_path).map_err(|source| NgcError::Io {
                path: cert_path.to_path_buf(),
                source,
            })?;
            Ok(Some(TlsConfig::from_pem(cert_pem, key_pem)))
        }
        (None, None) => Ok(Some(TlsConfig::self_signed(&[host.to_string()])?)),
        (Some(_), None) => Err(NgcError::ServeError {
            message: "--ssl-key was set without --ssl-cert; both are required".to_string(),
        }),
        (None, Some(_)) => Err(NgcError::ServeError {
            message: "--ssl-cert was set without --ssl-key; both are required".to_string(),
        }),
    }
}

/// Variant of [`run`] that lets the caller decide how the shutdown flag is
/// armed. Tests use a no-op installer so the watcher loop can be exited via
/// the returned [`Arc<AtomicBool>`] without touching the real signal
/// machinery (which would interfere with `cargo test`'s own handlers).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with_stop(
    project: &Path,
    configuration: Option<&str>,
    host: &str,
    port: u16,
    open: bool,
    serve_path: Option<&str>,
    allowed_hosts: &[String],
    headers: &[(String, String)],
    ssl: bool,
    ssl_key: Option<&Path>,
    ssl_cert: Option<&Path>,
    hmr_override: Option<bool>,
    install_stop: impl FnOnce(Arc<AtomicBool>),
) -> NgcResult<()> {
    let tls = resolve_tls(ssl, ssl_key, ssl_cert, host)?;

    let out_dir = crate::resolve_out_dir(project, None, configuration)?;
    let mut cache = BuildCache::new();

    // Resolve HMR: CLI `--hmr`/`--no-hmr` wins; otherwise inherit
    // `architect.serve.options.hmr` from angular.json (default `false`).
    // Also collect the absolute paths of the global stylesheet entries so a
    // rebuild that touches only those can be classified as a CSS-only update.
    let resolved = crate::find_and_resolve_angular_json(project, configuration)?;
    let hmr_enabled = hmr_override.unwrap_or_else(|| resolved.as_ref().map(|p| p.hmr).unwrap_or(false));
    let global_style_paths: std::collections::HashSet<PathBuf> = resolved
        .as_ref()
        .map(|p| {
            p.styles
                .iter()
                .map(|s| canonical_or_owned(&s.path))
                .collect()
        })
        .unwrap_or_default();
    if hmr_enabled {
        eprintln!("{}", "ngc-rs HMR enabled".bold().green());
    }

    // Normalize the user-supplied servePath up-front so the dev server
    // mount and the index.html `<base href>` fallback agree on the
    // canonical `/foo/` form (see `ngc_dev_server::normalize_serve_path`).
    let normalized_serve_path = serve_path.and_then(ngc_dev_server::normalize_serve_path);

    // `serve` is dev-only; pass `strict_templates: false` so JIT fallback
    // warnings are not escalated into a build error that would block dev
    // iteration on transient template-compile gaps.
    let initial = crate::run_build_with_options(
        project,
        None,
        configuration,
        None,
        false,
        Some(&mut cache),
        normalized_serve_path.as_deref(),
        hmr_enabled,
    )?;
    eprintln!(
        "{} {} module(s), {} file(s)",
        "ngc-rs build complete".bold().green(),
        initial.modules_bundled,
        initial.output_files.len(),
    );

    // Shared registry of per-component HMR update modules served at
    // `/@ng/component`. Seed it from the initial build; the rebuild callback
    // replaces its contents each cycle.
    let component_updates: ComponentUpdates = Arc::new(Mutex::new(HashMap::new()));
    if hmr_enabled {
        if let Ok(mut map) = component_updates.lock() {
            map.clone_from(&initial.hmr_component_updates);
        }
    }

    // When HMR is on, bind `import.meta.hot` inside the entry module so the
    // per-component initializers can register update handlers.
    if hmr_enabled {
        inject_hmr_runtime(&out_dir);
    }

    let (event_tx, event_rx) = channel::<DevServerEvent>();
    let cfg = DevServerConfig::new(&out_dir)
        .with_host(host.to_string())
        .with_port(port)
        .with_serve_path(normalized_serve_path.as_deref())
        .with_allowed_hosts(allowed_hosts.iter().cloned())
        .with_headers(headers.iter().cloned())
        .with_tls(tls)
        .with_component_updates(Arc::clone(&component_updates));
    let server = DevServer::start(cfg, event_rx)?;
    let scheme = server.scheme();
    let url = match server.serve_path() {
        Some(prefix) => format!("{scheme}://{}{}", server.addr(), prefix),
        None => format!("{scheme}://{}", server.addr()),
    };
    eprintln!(
        "{} {}",
        "ngc-rs serve listening on".bold().green(),
        url.as_str()
    );

    if open {
        if let Err(e) = open_browser(&url) {
            tracing::warn!(error = %e, "could not open browser");
        }
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    install_stop(Arc::clone(&shutdown));

    let watcher = Watcher::new(WatcherConfig::new(watch_root(project)));
    let project_path = project.to_path_buf();
    let configuration_owned = configuration.map(|s| s.to_string());
    let serve_path_owned = normalized_serve_path.clone();
    let out_dir_owned = out_dir.clone();
    let registry = Arc::clone(&component_updates);
    // Monotonic cache-buster for swapped stylesheets and component-update
    // fetches; must change every rebuild so the browser re-fetches.
    let mut hmr_tick: u64 = 0;

    let build_fn = move |dirty: &[PathBuf]| -> NgcResult<()> {
        if dirty.iter().any(|p| !is_ts_path(p)) {
            cache.clear();
        } else {
            cache.invalidate(dirty);
        }
        let outcome = crate::run_build_with_options(
            &project_path,
            None,
            configuration_owned.as_deref(),
            None,
            false,
            Some(&mut cache),
            serve_path_owned.as_deref(),
            hmr_enabled,
        );
        match outcome {
            Ok(result) => {
                eprintln!(
                    "{} {} module(s), {} dirty",
                    "ngc-rs rebuild".bold().green(),
                    result.modules_bundled,
                    dirty.len()
                );
                if hmr_enabled {
                    // Re-bind `import.meta.hot` in the freshly written entry
                    // chunk and refresh the served update-module registry.
                    inject_hmr_runtime(&out_dir_owned);
                    if let Ok(mut map) = registry.lock() {
                        map.clone_from(&result.hmr_component_updates);
                    }
                }
                hmr_tick += 1;
                for event in classify_rebuild(
                    hmr_enabled,
                    dirty,
                    &global_style_paths,
                    &result.hmr_resource_to_component,
                    hmr_tick,
                ) {
                    if event_tx.send(event).is_err() {
                        tracing::debug!("dev server event channel closed");
                        break;
                    }
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("{} {e}", "ngc-rs rebuild failed:".bold().red());
                if event_tx.send(build_failure_event(&e)).is_err() {
                    tracing::debug!("dev server event channel closed");
                }
                Err(e)
            }
        }
    };

    let stop_flag = Arc::clone(&shutdown);
    let should_stop = move |_completed: usize| -> bool { stop_flag.load(Ordering::SeqCst) };

    let result = watcher.run_until(build_fn, should_stop);
    eprintln!("{}", "ngc-rs serve shutting down".dimmed());
    drop(server);
    result
}

/// Translate an [`NgcError`] into the [`DevServerEvent::BuildFailed`]
/// payload that ships to the browser overlay.
///
/// Reuses `Display` for the `message` so the overlay shows the same text
/// the terminal already prints, and lifts the offending file path plus
/// (when the underlying parser surfaced them) line/column coordinates.
pub(crate) fn build_failure_event(err: &NgcError) -> DevServerEvent {
    let (file, line, column) = error_location(err);
    DevServerEvent::BuildFailed {
        message: err.to_string(),
        file,
        line,
        column,
    }
}

/// Canonicalize `path`, falling back to its owned form when canonicalization
/// fails (e.g. the file was deleted between resolve and compare). Used so the
/// watcher's emitted paths and the resolved style paths compare equal even
/// across symlinks.
fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Prepend the HMR runtime prelude to every emitted JS chunk so the
/// per-component HMR initializers — which live in `main.js` for eager
/// components and in lazy `chunk-*.js` for routed ones — can resolve
/// `import.meta.hot` (each ES module has its own `import.meta`). The build
/// rewrites the chunks from scratch each cycle, so this runs after every
/// successful build. A per-file guard skips the work if the prelude is already
/// present. Failures are logged and ignored: a chunk that can't be patched
/// just degrades that component to live reload rather than breaking the app.
fn inject_hmr_runtime(out_dir: &Path) {
    let entries = match std::fs::read_dir(out_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(path = %out_dir.display(), error = %e, "could not read out_dir to inject HMR runtime");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Top-level `.js` chunks only (skip source maps and nested locale dirs).
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let Ok(existing) = std::fs::read_to_string(&path) else {
            continue;
        };
        if existing.starts_with(HMR_RUNTIME_PRELUDE) {
            continue;
        }
        let mut patched = String::with_capacity(HMR_RUNTIME_PRELUDE.len() + existing.len());
        patched.push_str(HMR_RUNTIME_PRELUDE);
        patched.push_str(&existing);
        if let Err(e) = std::fs::write(&path, patched) {
            tracing::debug!(path = %path.display(), error = %e, "could not inject HMR runtime into chunk");
        }
    }
}

/// True when a rebuild's `dirty` set is non-empty and every changed file is a
/// global stylesheet entry — the case where HMR can swap `styles.css` in place
/// instead of reloading. An empty set (or any non-stylesheet change) returns
/// `false`, falling back to a full reload.
fn is_global_css_only_change(
    dirty: &[PathBuf],
    global_style_paths: &std::collections::HashSet<PathBuf>,
) -> bool {
    !dirty.is_empty()
        && dirty
            .iter()
            .all(|p| global_style_paths.contains(&canonical_or_owned(p)))
}

/// Decide which dev-server event(s) a successful rebuild should fan out.
///
/// * HMR off → always a full [`DevServerEvent::Reload`].
/// * Every dirty file is a global stylesheet → one [`DevServerEvent::CssUpdate`].
/// * Every dirty file is an external component resource (`templateUrl` /
///   `styleUrls`) of a known component → one [`DevServerEvent::ComponentUpdate`]
///   per affected component, swapping it in place.
/// * Anything else (a `.ts` class change, an inline-template edit, or an
///   unrecognised file) → a full reload. Inline-template/`.ts` HMR is
///   intentionally deferred to a follow-up.
fn classify_rebuild(
    hmr_enabled: bool,
    dirty: &[PathBuf],
    global_style_paths: &std::collections::HashSet<PathBuf>,
    resource_to_component: &HashMap<PathBuf, String>,
    timestamp: u64,
) -> Vec<DevServerEvent> {
    if !hmr_enabled {
        return vec![DevServerEvent::Reload];
    }
    if is_global_css_only_change(dirty, global_style_paths) {
        return vec![DevServerEvent::CssUpdate { timestamp }];
    }
    if dirty.is_empty() {
        return vec![DevServerEvent::Reload];
    }
    let mut ids: Vec<String> = Vec::new();
    for path in dirty {
        match resource_to_component.get(&canonical_or_owned(path)) {
            Some(id) => {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            // A `.ts` change, inline-template edit, or unknown file → reload.
            None => return vec![DevServerEvent::Reload],
        }
    }
    ids.into_iter()
        .map(|id| DevServerEvent::ComponentUpdate { id, timestamp })
        .collect()
}

fn error_location(err: &NgcError) -> (Option<PathBuf>, Option<u32>, Option<u32>) {
    match err {
        NgcError::ParseError {
            path, line, column, ..
        }
        | NgcError::TransformError {
            path, line, column, ..
        }
        | NgcError::TemplateParseError {
            path, line, column, ..
        }
        | NgcError::TemplateCompileError {
            path, line, column, ..
        }
        | NgcError::LinkerError {
            path, line, column, ..
        } => (Some(path.clone()), *line, *column),
        NgcError::Io { path, .. }
        | NgcError::TsConfigParse { path, .. }
        | NgcError::TsConfigExtendsNotFound { path }
        | NgcError::AngularJsonParse { path, .. }
        | NgcError::AssetError { path, .. }
        | NgcError::StyleError { path, .. }
        | NgcError::SourceMapError { path, .. }
        | NgcError::MinifyError { path, .. } => (Some(path.clone()), None, None),
        NgcError::UnresolvedImport { from_file, .. } => (Some(from_file.clone()), None, None),
        _ => (None, None, None),
    }
}

fn install_ctrlc(flag: Arc<AtomicBool>) {
    if let Err(e) = ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    }) {
        tracing::warn!(error = %e, "could not install Ctrl+C handler");
    }
}

/// Spawn the platform-native browser-opening command for `url`.
///
/// macOS uses `open`, Linux uses `xdg-open`, and Windows uses
/// `cmd /c start`. Any failure is surfaced as an [`NgcError::ServeError`]
/// so the caller can log it and continue serving — the server is useful
/// even when the browser handoff fails.
pub(crate) fn open_browser(url: &str) -> NgcResult<()> {
    let mut cmd = browser_command();
    cmd.arg(url);
    cmd.spawn().map(|_| ()).map_err(|e| NgcError::ServeError {
        message: format!("could not spawn browser opener: {e}"),
    })
}

#[cfg(target_os = "macos")]
fn browser_command() -> std::process::Command {
    std::process::Command::new("open")
}

#[cfg(target_os = "linux")]
fn browser_command() -> std::process::Command {
    std::process::Command::new("xdg-open")
}

#[cfg(target_os = "windows")]
fn browser_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/c", "start", ""]);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn browser_command() -> std::process::Command {
    std::process::Command::new("xdg-open")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn install_ctrlc_does_not_panic_when_handler_already_set() {
        // Calling twice is allowed; the second call returns Err which we
        // log and swallow. Just exercise the path.
        let flag = Arc::new(AtomicBool::new(false));
        install_ctrlc(Arc::clone(&flag));
        install_ctrlc(flag);
    }

    #[test]
    fn browser_command_is_constructible() {
        let cmd = browser_command();
        // We don't run it; just confirm the constructor returned a Command
        // with a non-empty program name on the host platform.
        assert!(!cmd.get_program().is_empty());
    }

    #[test]
    fn build_failure_event_extracts_location_from_parse_error() {
        let err = NgcError::ParseError {
            path: PathBuf::from("/proj/src/app.ts"),
            message: "unexpected token".to_string(),
            line: Some(12),
            column: Some(3),
        };
        let DevServerEvent::BuildFailed {
            message,
            file,
            line,
            column,
        } = build_failure_event(&err)
        else {
            panic!("expected BuildFailed variant");
        };
        assert!(message.contains("unexpected token"));
        assert_eq!(file.as_deref(), Some(Path::new("/proj/src/app.ts")));
        assert_eq!(line, Some(12));
        assert_eq!(column, Some(3));
    }

    #[test]
    fn build_failure_event_omits_location_when_parser_did_not_supply_one() {
        let err = NgcError::ParseError {
            path: PathBuf::from("/proj/src/app.ts"),
            message: "unsupported file extension".to_string(),
            line: None,
            column: None,
        };
        let DevServerEvent::BuildFailed { line, column, .. } = build_failure_event(&err) else {
            panic!("expected BuildFailed variant");
        };
        assert!(line.is_none());
        assert!(column.is_none());
    }

    #[test]
    fn build_failure_event_extracts_from_file_for_unresolved_import() {
        let err = NgcError::UnresolvedImport {
            specifier: "./missing".to_string(),
            from_file: PathBuf::from("/proj/src/app.ts"),
        };
        let DevServerEvent::BuildFailed { file, .. } = build_failure_event(&err) else {
            panic!("expected BuildFailed variant");
        };
        assert_eq!(file.as_deref(), Some(Path::new("/proj/src/app.ts")));
    }

    #[test]
    fn resolve_tls_disabled_returns_none() {
        assert!(resolve_tls(false, None, None, "localhost")
            .expect("ok")
            .is_none());
    }

    #[test]
    fn resolve_tls_rejects_key_or_cert_without_ssl() {
        assert!(resolve_tls(false, Some(Path::new("/k")), None, "localhost").is_err());
        assert!(resolve_tls(false, None, Some(Path::new("/c")), "localhost").is_err());
    }

    #[test]
    fn resolve_tls_auto_generates_when_ssl_without_paths() {
        let tls = resolve_tls(true, None, None, "localhost")
            .expect("ok")
            .expect("some tls");
        // Round-trips through the dev server's SslConfig as PEM bytes; just
        // confirm something was minted.
        let _ = tls; // opaque material; presence is the assertion
    }

    #[test]
    fn resolve_tls_requires_both_key_and_cert() {
        assert!(resolve_tls(true, Some(Path::new("/k")), None, "localhost").is_err());
        assert!(resolve_tls(true, None, Some(Path::new("/c")), "localhost").is_err());
    }

    #[test]
    fn resolve_tls_reads_explicit_pem_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("dev.key");
        let cert = dir.path().join("dev.crt");
        std::fs::write(&key, b"KEYDATA").expect("write key");
        std::fs::write(&cert, b"CERTDATA").expect("write cert");
        let tls = resolve_tls(true, Some(&key), Some(&cert), "localhost")
            .expect("ok")
            .expect("some tls");
        let _ = tls;
    }

    #[test]
    fn resolve_tls_errors_when_explicit_pem_missing() {
        let err = resolve_tls(
            true,
            Some(Path::new("/no/such/key.pem")),
            Some(Path::new("/no/such/cert.pem")),
            "localhost",
        )
        .expect_err("missing file should error");
        assert!(matches!(err, NgcError::Io { .. }));
    }

    #[test]
    fn css_only_change_classification() {
        use std::collections::HashSet;
        let styles: HashSet<PathBuf> = [PathBuf::from("/proj/src/styles.css"), PathBuf::from("/proj/src/theme.scss")]
            .into_iter()
            .collect();

        // All dirty files are global stylesheets → CSS-only.
        assert!(is_global_css_only_change(
            &[PathBuf::from("/proj/src/styles.css")],
            &styles
        ));
        assert!(is_global_css_only_change(
            &[
                PathBuf::from("/proj/src/styles.css"),
                PathBuf::from("/proj/src/theme.scss")
            ],
            &styles
        ));

        // A non-stylesheet change (or a stylesheet not in the global set)
        // forces a full reload.
        assert!(!is_global_css_only_change(
            &[PathBuf::from("/proj/src/app.component.ts")],
            &styles
        ));
        assert!(!is_global_css_only_change(
            &[
                PathBuf::from("/proj/src/styles.css"),
                PathBuf::from("/proj/src/app.component.ts")
            ],
            &styles
        ));
        assert!(!is_global_css_only_change(
            &[PathBuf::from("/proj/src/app.component.css")],
            &styles
        ));

        // Empty dirty set never qualifies.
        assert!(!is_global_css_only_change(&[], &styles));
    }

    #[test]
    fn classify_rebuild_routes_events() {
        use std::collections::{HashMap, HashSet};
        let styles: HashSet<PathBuf> = [PathBuf::from("/proj/src/styles.css")].into_iter().collect();
        let mut resources: HashMap<PathBuf, String> = HashMap::new();
        resources.insert(PathBuf::from("/proj/src/app/app.component.html"), "id-app".to_string());
        resources.insert(PathBuf::from("/proj/src/app/app.component.css"), "id-app".to_string());
        resources.insert(PathBuf::from("/proj/src/app/foo.component.html"), "id-foo".to_string());

        // HMR off → always reload.
        assert!(matches!(
            classify_rebuild(false, &[PathBuf::from("/proj/src/app/app.component.html")], &styles, &resources, 1).as_slice(),
            [DevServerEvent::Reload]
        ));

        // Global stylesheet only → CssUpdate.
        assert!(matches!(
            classify_rebuild(true, &[PathBuf::from("/proj/src/styles.css")], &styles, &resources, 5).as_slice(),
            [DevServerEvent::CssUpdate { timestamp: 5 }]
        ));

        // A component template → one ComponentUpdate for its id.
        let evs = classify_rebuild(true, &[PathBuf::from("/proj/src/app/app.component.html")], &styles, &resources, 7);
        match evs.as_slice() {
            [DevServerEvent::ComponentUpdate { id, timestamp: 7 }] => assert_eq!(id, "id-app"),
            other => panic!("expected one ComponentUpdate, got {other:?}"),
        }

        // Two resources of the same component → deduped to a single update.
        let evs = classify_rebuild(
            true,
            &[
                PathBuf::from("/proj/src/app/app.component.html"),
                PathBuf::from("/proj/src/app/app.component.css"),
            ],
            &styles,
            &resources,
            9,
        );
        assert_eq!(evs.len(), 1);

        // Two distinct components → one update each.
        let evs = classify_rebuild(
            true,
            &[
                PathBuf::from("/proj/src/app/app.component.html"),
                PathBuf::from("/proj/src/app/foo.component.html"),
            ],
            &styles,
            &resources,
            9,
        );
        assert_eq!(evs.len(), 2);

        // A `.ts` (or any unknown) change → reload, even mixed with a resource.
        assert!(matches!(
            classify_rebuild(true, &[PathBuf::from("/proj/src/app/app.component.ts")], &styles, &resources, 1).as_slice(),
            [DevServerEvent::Reload]
        ));
        assert!(matches!(
            classify_rebuild(
                true,
                &[
                    PathBuf::from("/proj/src/app/app.component.html"),
                    PathBuf::from("/proj/src/app/app.component.ts"),
                ],
                &styles,
                &resources,
                1
            )
            .as_slice(),
            [DevServerEvent::Reload]
        ));

        // Empty dirty set → reload.
        assert!(matches!(
            classify_rebuild(true, &[], &styles, &resources, 1).as_slice(),
            [DevServerEvent::Reload]
        ));
    }

    #[test]
    fn build_failure_event_omits_path_for_pathless_errors() {
        let err = NgcError::ServeError {
            message: "boom".to_string(),
        };
        let DevServerEvent::BuildFailed {
            message,
            file,
            line,
            column,
        } = build_failure_event(&err)
        else {
            panic!("expected BuildFailed variant");
        };
        assert!(message.contains("boom"));
        assert!(file.is_none());
        assert!(line.is_none());
        assert!(column.is_none());
    }
}
