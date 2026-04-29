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

use colored::Colorize;
use ngc_dev_server::{DevServer, DevServerConfig, ReloadEvent};
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_watch::{Watcher, WatcherConfig};

use crate::incremental::BuildCache;
use crate::watch_cmd::{is_ts_path, watch_root};

/// Run the `serve` subcommand: bring up the dev server, drive the watcher,
/// and block until Ctrl+C.
pub fn run(
    project: &Path,
    configuration: Option<&str>,
    host: &str,
    port: u16,
    open: bool,
) -> NgcResult<()> {
    run_with_stop(project, configuration, host, port, open, install_ctrlc)
}

/// Variant of [`run`] that lets the caller decide how the shutdown flag is
/// armed. Tests use a no-op installer so the watcher loop can be exited via
/// the returned [`Arc<AtomicBool>`] without touching the real signal
/// machinery (which would interfere with `cargo test`'s own handlers).
pub(crate) fn run_with_stop(
    project: &Path,
    configuration: Option<&str>,
    host: &str,
    port: u16,
    open: bool,
    install_stop: impl FnOnce(Arc<AtomicBool>),
) -> NgcResult<()> {
    let out_dir = crate::resolve_out_dir(project, None, configuration)?;
    let mut cache = BuildCache::new();

    let initial =
        crate::run_build_with_cache(project, None, configuration, false, Some(&mut cache))?;
    eprintln!(
        "{} {} module(s), {} file(s)",
        "ngc-rs build complete".bold().green(),
        initial.modules_bundled,
        initial.output_files.len(),
    );

    let (reload_tx, reload_rx) = channel::<ReloadEvent>();
    let cfg = DevServerConfig::new(&out_dir)
        .with_host(host.to_string())
        .with_port(port);
    let server = DevServer::start(cfg, reload_rx)?;
    let url = format!("http://{}", server.addr());
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

    let build_fn = move |dirty: &[PathBuf]| -> NgcResult<()> {
        if dirty.iter().any(|p| !is_ts_path(p)) {
            cache.clear();
        } else {
            cache.invalidate(dirty);
        }
        let outcome = crate::run_build_with_cache(
            &project_path,
            None,
            configuration_owned.as_deref(),
            false,
            Some(&mut cache),
        );
        match outcome {
            Ok(result) => {
                eprintln!(
                    "{} {} module(s), {} dirty",
                    "ngc-rs rebuild".bold().green(),
                    result.modules_bundled,
                    dirty.len()
                );
                if reload_tx.send(ReloadEvent).is_err() {
                    tracing::debug!("dev server reload channel closed");
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("{} {e}", "ngc-rs rebuild failed:".bold().red());
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
}
