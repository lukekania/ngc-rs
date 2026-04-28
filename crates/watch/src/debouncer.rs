use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Tunables for [`Debouncer`].
#[derive(Debug, Clone)]
pub struct DebouncerConfig {
    /// How long to wait after the last observed event before flushing the
    /// accumulated dirty set. Editors like VSCode write a file by replacing
    /// it with a temp + rename, which produces several events in a row;
    /// 100 ms is enough to coalesce those without being user-visible.
    pub debounce: Duration,
    /// Hard upper bound on how long a single dirty path may sit in the
    /// buffer. Without this, a workflow that writes the same file every
    /// 50 ms would never trigger a rebuild.
    pub max_wait: Duration,
}

impl Default for DebouncerConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_millis(500),
        }
    }
}

/// Coalesces a stream of file-change notifications into discrete rebuild
/// batches. Pure logic — no threads, no I/O — so it can be unit-tested with
/// an injected clock.
///
/// Caller pattern:
///   1. Push every file path the watcher reports via [`Debouncer::push`].
///   2. After each push, ask [`Debouncer::should_flush`] whether the window
///      has closed; if so, drain the dirty set with [`Debouncer::take`].
#[derive(Debug)]
pub struct Debouncer {
    config: DebouncerConfig,
    dirty: BTreeSet<PathBuf>,
    /// `Some(t)` once a path is pending; `None` after a flush.
    first_seen: Option<Instant>,
    /// Updated every time a new path arrives.
    last_seen: Option<Instant>,
}

impl Debouncer {
    /// Build a debouncer with the given window settings.
    pub fn new(config: DebouncerConfig) -> Self {
        Self {
            config,
            dirty: BTreeSet::new(),
            first_seen: None,
            last_seen: None,
        }
    }

    /// Convenience for the default 100 ms debounce / 500 ms max-wait window.
    pub fn with_defaults() -> Self {
        Self::new(DebouncerConfig::default())
    }

    /// Record a file as dirty at `now`.
    pub fn push(&mut self, path: PathBuf, now: Instant) {
        if self.first_seen.is_none() {
            self.first_seen = Some(now);
        }
        self.last_seen = Some(now);
        self.dirty.insert(path);
    }

    /// `true` when the dirty set should be flushed: either the quiet window
    /// has elapsed since the last event, or the max-wait cap has been hit.
    pub fn should_flush(&self, now: Instant) -> bool {
        match (self.first_seen, self.last_seen) {
            (Some(first), Some(last)) => {
                let quiet = now.saturating_duration_since(last) >= self.config.debounce;
                let capped = now.saturating_duration_since(first) >= self.config.max_wait;
                !self.dirty.is_empty() && (quiet || capped)
            }
            _ => false,
        }
    }

    /// Drain the accumulated dirty set and reset the window. Returns an
    /// empty vec when nothing is pending.
    pub fn take(&mut self) -> Vec<PathBuf> {
        self.first_seen = None;
        self.last_seen = None;
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// `true` once at least one path has been pushed since the last flush.
    pub fn is_pending(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Number of paths currently buffered.
    pub fn pending_len(&self) -> usize {
        self.dirty.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn empty_debouncer_does_not_flush() {
        let d = Debouncer::with_defaults();
        assert!(!d.should_flush(Instant::now()));
        assert!(!d.is_pending());
    }

    #[test]
    fn flush_after_quiet_window() {
        let base = Instant::now();
        let mut d = Debouncer::new(DebouncerConfig {
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_millis(1000),
        });
        d.push(PathBuf::from("a.ts"), at(base, 0));
        assert!(!d.should_flush(at(base, 50)));
        assert!(d.should_flush(at(base, 200)));
        let taken = d.take();
        assert_eq!(taken, vec![PathBuf::from("a.ts")]);
        assert!(!d.is_pending());
    }

    #[test]
    fn coalesces_repeated_pushes() {
        let base = Instant::now();
        let mut d = Debouncer::new(DebouncerConfig {
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_millis(1000),
        });
        d.push(PathBuf::from("a.ts"), at(base, 0));
        d.push(PathBuf::from("a.ts"), at(base, 30));
        d.push(PathBuf::from("a.ts"), at(base, 60));
        assert_eq!(d.pending_len(), 1);
        // Each push extends the quiet window, so 80 ms after t=0 we are
        // still within 100 ms of the last (t=60) push.
        assert!(!d.should_flush(at(base, 80)));
        assert!(d.should_flush(at(base, 200)));
    }

    #[test]
    fn max_wait_caps_continuous_edits() {
        let base = Instant::now();
        let mut d = Debouncer::new(DebouncerConfig {
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_millis(250),
        });
        for i in 0..10 {
            d.push(PathBuf::from("a.ts"), at(base, i * 50));
        }
        // The last push is at t=450 ms but the first was at t=0, so max_wait
        // (250 ms) is exceeded — flush even though pushes are still arriving
        // within the debounce window.
        assert!(d.should_flush(at(base, 460)));
    }

    #[test]
    fn take_resets_window() {
        let base = Instant::now();
        let mut d = Debouncer::with_defaults();
        d.push(PathBuf::from("a.ts"), at(base, 0));
        let _ = d.take();
        assert!(!d.is_pending());
        assert!(!d.should_flush(at(base, 5_000)));
        d.push(PathBuf::from("b.ts"), at(base, 5_000));
        assert!(d.is_pending());
    }
}
