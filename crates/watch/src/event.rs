use std::path::PathBuf;
use std::sync::Arc;

/// An event broadcast by [`crate::Watcher`] as it observes filesystem
/// changes and drives rebuilds.
///
/// Subscribers (the dev-server, the CLI's status output, tests) receive these
/// events in the watcher thread; handlers must be cheap and non-blocking.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// One or more files matching the watched filter changed on disk. Emitted
    /// once per debounce window with the deduplicated list of dirty paths,
    /// before the rebuild runs.
    FilesChanged {
        /// Canonicalized paths of files that changed during the window.
        paths: Vec<PathBuf>,
    },
    /// A rebuild started for the supplied dirty set.
    RebuildStarted {
        /// Files driving this rebuild.
        changed: Vec<PathBuf>,
    },
    /// A rebuild finished successfully.
    RebuildCompleted {
        /// Files that drove this rebuild.
        changed: Vec<PathBuf>,
        /// Wall-clock duration of the rebuild in milliseconds.
        duration_ms: u128,
    },
    /// A rebuild failed; `message` is the user-facing error string.
    RebuildFailed {
        /// Files that drove the failed rebuild.
        changed: Vec<PathBuf>,
        /// Display string from the underlying [`ngc_diagnostics::NgcError`].
        message: String,
    },
}

/// A trait for components that consume [`WatchEvent`]s.
///
/// Implementations are stored as `Arc<dyn WatchSubscriber>` so the watcher
/// can fan an event out to every subscriber synchronously. Implementations
/// must be `Send + Sync`; handlers run on the watcher thread, so callers
/// should hand off any expensive work onto a worker (e.g. via rayon).
pub trait WatchSubscriber: Send + Sync {
    /// Receive a single event. The default debouncer guarantees at most one
    /// `FilesChanged` per debounce window, but rebuild events fire freely.
    fn on_event(&self, event: &WatchEvent);
}

/// A no-op subscriber, useful as a placeholder in tests and for callers that
/// only consume the rebuild result via the build callback.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSubscriber;

impl WatchSubscriber for NoopSubscriber {
    fn on_event(&self, _event: &WatchEvent) {}
}

/// Build a list of subscribers from heterogeneous concrete types. Helper for
/// callers that want to attach several subscribers at construction time.
pub fn subscribers<I>(iter: I) -> Vec<Arc<dyn WatchSubscriber>>
where
    I: IntoIterator<Item = Arc<dyn WatchSubscriber>>,
{
    iter.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
    }

    impl WatchSubscriber for Recorder {
        fn on_event(&self, event: &WatchEvent) {
            let tag = match event {
                WatchEvent::FilesChanged { paths } => format!("changed:{}", paths.len()),
                WatchEvent::RebuildStarted { .. } => "started".to_string(),
                WatchEvent::RebuildCompleted { .. } => "completed".to_string(),
                WatchEvent::RebuildFailed { .. } => "failed".to_string(),
            };
            if let Ok(mut events) = self.events.lock() {
                events.push(tag);
            }
        }
    }

    #[test]
    fn noop_subscriber_does_not_panic() {
        let s = NoopSubscriber;
        s.on_event(&WatchEvent::RebuildStarted {
            changed: vec![PathBuf::from("a.ts")],
        });
    }

    #[test]
    fn subscribers_helper_collects_arc_pointers() {
        let recorder: Arc<dyn WatchSubscriber> = Arc::new(Recorder::default());
        let collected = subscribers([recorder.clone(), Arc::new(NoopSubscriber) as _]);
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn recorder_subscriber_captures_event_tags() {
        let recorder = Arc::new(Recorder::default());
        let dyn_recorder: Arc<dyn WatchSubscriber> = recorder.clone();
        dyn_recorder.on_event(&WatchEvent::FilesChanged {
            paths: vec![PathBuf::from("a.ts"), PathBuf::from("b.ts")],
        });
        dyn_recorder.on_event(&WatchEvent::RebuildCompleted {
            changed: vec![PathBuf::from("a.ts")],
            duration_ms: 12,
        });
        let events = recorder.events.lock().expect("recorder mutex poisoned");
        assert_eq!(
            *events,
            vec!["changed:2".to_string(), "completed".to_string()]
        );
    }

    #[test]
    fn watch_event_clone_preserves_paths() {
        let original = WatchEvent::RebuildFailed {
            changed: vec![PathBuf::from("a.ts")],
            message: "boom".to_string(),
        };
        let cloned = original.clone();
        match cloned {
            WatchEvent::RebuildFailed { changed, message } => {
                assert_eq!(changed, vec![PathBuf::from("a.ts")]);
                assert_eq!(message, "boom");
            }
            _ => panic!("clone changed variant"),
        }
    }
}
