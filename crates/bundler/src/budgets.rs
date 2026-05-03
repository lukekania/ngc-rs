//! Size-budget enforcement for emitted bundles.
//!
//! Mirrors `@angular/build:application`'s `budgets` array semantics so a
//! drop-in replacement doesn't silently ship over-size bundles. Given a
//! list of `ResolvedBudget`s and the set of emitted output artifacts,
//! [`evaluate`] returns every threshold violation as a [`BudgetViolation`].
//! The cli wires those into `BuildResult.errors` / `BuildResult.warnings`
//! and fails the build (exit 1) on any error-severity violation.
//!
//! Pure functions — no IO, no panics — so the unit tests can exercise the
//! whole matrix without setting up a fixture project.

use std::path::Path;

use ngc_project_resolver::angular_json::{BudgetKind, ResolvedBudget};

/// Coarse classification of an output file. Determines which budgets the
/// file counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// JavaScript bundle (`.js`, `.mjs`, `.cjs`). Counts toward `Initial`
    /// (when initial), `AnyScript`, `AllScript`, `All`, and named `Bundle`.
    Script,
    /// Stylesheet (`.css`). Counts toward `Initial` (when initial),
    /// `AnyComponentStyle`, `All`, and named `Bundle`.
    Style,
    /// Anything else (source maps, HTML, assets). Excluded from every
    /// budget.
    Other,
}

impl FileKind {
    /// Classify a path by extension.
    pub fn from_path(p: &Path) -> Self {
        match p.extension().and_then(|e| e.to_str()) {
            Some("js" | "mjs" | "cjs") => FileKind::Script,
            Some("css") => FileKind::Style,
            _ => FileKind::Other,
        }
    }
}

/// One emitted artifact, normalised for budget evaluation.
#[derive(Debug, Clone)]
pub struct OutputArtifact<'a> {
    /// Path to the file (used only for diagnostics/display).
    pub path: &'a Path,
    /// Bundle name without content hash or extension. For `main.abc123.js`
    /// this is `"main"`; for `chunk-foo.def456.js` this is `"chunk-foo"`.
    /// Used to match `Bundle`-kind budgets that target a named bundle.
    pub bundle_name: String,
    /// Size of the file in bytes.
    pub size: u64,
    /// Coarse classification.
    pub kind: FileKind,
    /// `true` when this file is part of the *initial* download —
    /// `main`, `polyfills`, and global stylesheets. `false` for
    /// lazy-loaded chunks. Drives `Initial`-kind budget summation.
    pub is_initial: bool,
}

/// One budget threshold violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetViolation {
    /// Which budget was tripped.
    pub kind: BudgetKind,
    /// Bundle name this violation is for (only set for `AnyScript`,
    /// `AnyComponentStyle`, and `Bundle`-kind budgets that scope per file;
    /// `None` for whole-app aggregates like `Initial`/`All`/`AllScript`).
    pub bundle_name: Option<String>,
    /// Actual size measured.
    pub actual_size: u64,
    /// Threshold that was exceeded.
    pub threshold: u64,
    /// Whether this should fail the build.
    pub severity: ViolationSeverity,
}

/// Whether a budget violation is fatal or advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationSeverity {
    /// Tripped `maximumWarning` — surfaced to the user but the build
    /// completes.
    Warning,
    /// Tripped `maximumError` — fails the build (exit 1).
    Error,
}

/// Evaluate every budget against the given outputs and return all
/// violations in declaration order. Per-file budgets (`AnyScript`,
/// `AnyComponentStyle`, `Bundle`) emit one violation per offending file;
/// aggregate budgets (`Initial`, `All`, `AllScript`) emit at most one.
pub fn evaluate(
    budgets: &[ResolvedBudget],
    outputs: &[OutputArtifact<'_>],
) -> Vec<BudgetViolation> {
    let mut violations = Vec::new();
    for b in budgets {
        match b.kind {
            BudgetKind::Initial => {
                let total: u64 = outputs
                    .iter()
                    .filter(|o| {
                        o.is_initial && matches!(o.kind, FileKind::Script | FileKind::Style)
                    })
                    .map(|o| o.size)
                    .sum();
                push_aggregate(&mut violations, b, total, None);
            }
            BudgetKind::All => {
                let total: u64 = outputs
                    .iter()
                    .filter(|o| matches!(o.kind, FileKind::Script | FileKind::Style))
                    .map(|o| o.size)
                    .sum();
                push_aggregate(&mut violations, b, total, None);
            }
            BudgetKind::AllScript => {
                let total: u64 = outputs
                    .iter()
                    .filter(|o| o.kind == FileKind::Script)
                    .map(|o| o.size)
                    .sum();
                push_aggregate(&mut violations, b, total, None);
            }
            BudgetKind::AnyScript => {
                for o in outputs.iter().filter(|o| o.kind == FileKind::Script) {
                    push_aggregate(&mut violations, b, o.size, Some(o.bundle_name.clone()));
                }
            }
            BudgetKind::AnyComponentStyle => {
                for o in outputs.iter().filter(|o| o.kind == FileKind::Style) {
                    push_aggregate(&mut violations, b, o.size, Some(o.bundle_name.clone()));
                }
            }
            BudgetKind::Bundle => {
                let Some(target) = b.name.as_deref() else {
                    tracing::warn!("`Bundle` budget without a `name` field — skipped");
                    continue;
                };
                for o in outputs.iter().filter(|o| o.bundle_name == target) {
                    push_aggregate(&mut violations, b, o.size, Some(o.bundle_name.clone()));
                }
            }
        }
    }
    violations
}

fn push_aggregate(
    out: &mut Vec<BudgetViolation>,
    budget: &ResolvedBudget,
    actual: u64,
    bundle_name: Option<String>,
) {
    if let Some(threshold) = budget.maximum_error {
        if actual > threshold {
            out.push(BudgetViolation {
                kind: budget.kind,
                bundle_name: bundle_name.clone(),
                actual_size: actual,
                threshold,
                severity: ViolationSeverity::Error,
            });
            // When both warning and error trip on the same metric, only
            // the more severe one is reported (matches ng build).
            return;
        }
    }
    if let Some(threshold) = budget.maximum_warning {
        if actual > threshold {
            out.push(BudgetViolation {
                kind: budget.kind,
                bundle_name,
                actual_size: actual,
                threshold,
                severity: ViolationSeverity::Warning,
            });
        }
    }
}

/// Format a byte count the same way ng build does — three significant
/// digits, kibibyte/mebibyte units (kb/mb meaning ×1024). Used for the
/// human-readable budget table written to stderr.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} kB", b / KB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Render a one-line, human-readable description of a violation suitable
/// for stderr or `Diagnostic.message`.
pub fn format_violation(v: &BudgetViolation) -> String {
    let kind = match v.kind {
        BudgetKind::Initial => "initial",
        BudgetKind::AnyComponentStyle => "anyComponentStyle",
        BudgetKind::AnyScript => "anyScript",
        BudgetKind::Bundle => "bundle",
        BudgetKind::All => "all",
        BudgetKind::AllScript => "allScript",
    };
    let target = v
        .bundle_name
        .as_deref()
        .map(|n| format!(" ({n})"))
        .unwrap_or_default();
    let kind_label = match v.severity {
        ViolationSeverity::Error => "exceeded maximum budget",
        ViolationSeverity::Warning => "exceeded maximum warning budget",
    };
    let over = v.actual_size.saturating_sub(v.threshold);
    format!(
        "{kind} budget{target} {kind_label}: {} > {} (over by {})",
        format_bytes(v.actual_size),
        format_bytes(v.threshold),
        format_bytes(over),
    )
}

/// Strip a content hash and extension from a filename, returning the
/// "bundle name" used to match `Bundle`-kind budgets and to populate
/// [`OutputArtifact::bundle_name`].
///
/// Examples:
/// - `main.A1B2C3D4.js` → `main`
/// - `chunk-foo.DEADBEEF.js` → `chunk-foo`
/// - `polyfills.js` → `polyfills`
/// - `styles.css` → `styles`
/// - `vendor.js.map` → `vendor` (but `.map` files should be classified as
///   `Other` and excluded before reaching this helper)
pub fn bundle_name_from_filename(filename: &str) -> String {
    // Strip the final extension (`.js`, `.css`, etc.)
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    // If what remains has a trailing `.HEX` segment that looks like a
    // content hash (8+ hex chars), strip it too. `.map` and other
    // sub-extensions don't match because we only strip when the segment is
    // a hex string.
    if let Some((before, last)) = stem.rsplit_once('.') {
        if last.len() >= 6 && last.len() <= 32 && last.chars().all(|c| c.is_ascii_hexdigit()) {
            return before.to_string();
        }
    }
    stem.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn budget(
        kind: BudgetKind,
        warn: Option<u64>,
        err: Option<u64>,
        name: Option<&str>,
    ) -> ResolvedBudget {
        ResolvedBudget {
            kind,
            name: name.map(String::from),
            maximum_warning: warn,
            maximum_error: err,
        }
    }

    fn artifact<'a>(
        path: &'a PathBuf,
        bundle_name: &str,
        size: u64,
        kind: FileKind,
        is_initial: bool,
    ) -> OutputArtifact<'a> {
        OutputArtifact {
            path,
            bundle_name: bundle_name.to_string(),
            size,
            kind,
            is_initial,
        }
    }

    #[test]
    fn parses_hashed_main_chunk_to_main() {
        assert_eq!(bundle_name_from_filename("main.A1B2C3D4.js"), "main");
    }

    #[test]
    fn parses_hashed_chunk_with_dash_in_name() {
        assert_eq!(
            bundle_name_from_filename("chunk-foo-bar.DEADBEEF.js"),
            "chunk-foo-bar"
        );
    }

    #[test]
    fn parses_unhashed_filename() {
        assert_eq!(bundle_name_from_filename("polyfills.js"), "polyfills");
        assert_eq!(bundle_name_from_filename("styles.css"), "styles");
    }

    #[test]
    fn does_not_strip_non_hex_segment() {
        // `dev` is not hex; treat the whole stem as the bundle name.
        assert_eq!(bundle_name_from_filename("main.dev.js"), "main.dev");
    }

    #[test]
    fn classifies_extensions() {
        assert_eq!(FileKind::from_path(Path::new("main.js")), FileKind::Script);
        assert_eq!(FileKind::from_path(Path::new("main.mjs")), FileKind::Script);
        assert_eq!(
            FileKind::from_path(Path::new("styles.css")),
            FileKind::Style
        );
        assert_eq!(
            FileKind::from_path(Path::new("main.js.map")),
            FileKind::Other
        );
        assert_eq!(
            FileKind::from_path(Path::new("index.html")),
            FileKind::Other
        );
    }

    #[test]
    fn formats_bytes_with_three_significant_digits() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(2048), "2.00 kB");
        assert_eq!(format_bytes(1_572_864), "1.50 MB");
    }

    #[test]
    fn initial_budget_sums_initial_script_and_style_only() {
        let p1 = PathBuf::from("/dist/main.js");
        let p2 = PathBuf::from("/dist/styles.css");
        let p3 = PathBuf::from("/dist/chunk-lazy.js");
        let outputs = vec![
            artifact(&p1, "main", 600 * 1024, FileKind::Script, true),
            artifact(&p2, "styles", 200 * 1024, FileKind::Style, true),
            artifact(&p3, "chunk-lazy", 9_999_999, FileKind::Script, false),
        ];
        // Threshold 700 KiB; initial total is 800 KiB → trips error.
        let budgets = vec![budget(
            BudgetKind::Initial,
            Some(500 * 1024),
            Some(700 * 1024),
            None,
        )];
        let v = evaluate(&budgets, &outputs);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].severity, ViolationSeverity::Error);
        assert_eq!(v[0].actual_size, 800 * 1024);
        assert_eq!(v[0].threshold, 700 * 1024);
    }

    #[test]
    fn warning_only_when_below_error_threshold() {
        let p1 = PathBuf::from("/dist/main.js");
        let outputs = vec![artifact(&p1, "main", 600 * 1024, FileKind::Script, true)];
        let budgets = vec![budget(
            BudgetKind::Initial,
            Some(500 * 1024),
            Some(700 * 1024),
            None,
        )];
        let v = evaluate(&budgets, &outputs);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].severity, ViolationSeverity::Warning);
    }

    #[test]
    fn no_violation_when_within_thresholds() {
        let p1 = PathBuf::from("/dist/main.js");
        let outputs = vec![artifact(&p1, "main", 100 * 1024, FileKind::Script, true)];
        let budgets = vec![budget(
            BudgetKind::Initial,
            Some(500 * 1024),
            Some(700 * 1024),
            None,
        )];
        let v = evaluate(&budgets, &outputs);
        assert!(v.is_empty());
    }

    #[test]
    fn any_script_budget_emits_per_chunk_violations() {
        let p1 = PathBuf::from("/dist/main.js");
        let p2 = PathBuf::from("/dist/chunk-a.js");
        let p3 = PathBuf::from("/dist/chunk-b.js");
        let outputs = vec![
            artifact(&p1, "main", 100, FileKind::Script, true),
            artifact(&p2, "chunk-a", 600, FileKind::Script, false),
            artifact(&p3, "chunk-b", 200, FileKind::Script, false),
        ];
        let budgets = vec![budget(BudgetKind::AnyScript, Some(150), None, None)];
        let v = evaluate(&budgets, &outputs);
        // chunk-a (600) and chunk-b (200) trip; main (100) doesn't.
        assert_eq!(v.len(), 2);
        let names: Vec<_> = v.iter().filter_map(|x| x.bundle_name.as_deref()).collect();
        assert!(names.contains(&"chunk-a"));
        assert!(names.contains(&"chunk-b"));
    }

    #[test]
    fn bundle_budget_targets_named_bundle_only() {
        let p1 = PathBuf::from("/dist/main.js");
        let p2 = PathBuf::from("/dist/chunk-a.js");
        let outputs = vec![
            artifact(&p1, "main", 100, FileKind::Script, true),
            artifact(&p2, "chunk-a", 1_000, FileKind::Script, false),
        ];
        let budgets = vec![budget(BudgetKind::Bundle, None, Some(500), Some("chunk-a"))];
        let v = evaluate(&budgets, &outputs);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].bundle_name.as_deref(), Some("chunk-a"));
    }

    #[test]
    fn all_script_budget_sums_every_script() {
        let p1 = PathBuf::from("/dist/main.js");
        let p2 = PathBuf::from("/dist/chunk.js");
        let p3 = PathBuf::from("/dist/styles.css");
        let outputs = vec![
            artifact(&p1, "main", 400, FileKind::Script, true),
            artifact(&p2, "chunk", 400, FileKind::Script, false),
            artifact(&p3, "styles", 1_000_000, FileKind::Style, true),
        ];
        let budgets = vec![budget(BudgetKind::AllScript, None, Some(500), None)];
        let v = evaluate(&budgets, &outputs);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].actual_size, 800);
        // Should NOT have included the stylesheet.
    }

    #[test]
    fn missing_thresholds_yield_no_violations() {
        let p1 = PathBuf::from("/dist/main.js");
        let outputs = vec![artifact(&p1, "main", 9_999_999, FileKind::Script, true)];
        let budgets = vec![budget(BudgetKind::Initial, None, None, None)];
        let v = evaluate(&budgets, &outputs);
        assert!(v.is_empty());
    }

    #[test]
    fn format_violation_includes_actual_threshold_and_overage() {
        let v = BudgetViolation {
            kind: BudgetKind::Initial,
            bundle_name: None,
            actual_size: 800 * 1024,
            threshold: 700 * 1024,
            severity: ViolationSeverity::Error,
        };
        let s = format_violation(&v);
        assert!(s.contains("initial"));
        assert!(s.contains("800.00 kB"));
        assert!(s.contains("700.00 kB"));
        assert!(s.contains("over by 100.00 kB"));
    }
}
