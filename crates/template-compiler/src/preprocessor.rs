//! Component-style preprocessor harness for SCSS / Sass / Less / Stylus.
//!
//! Each language is backed by a single long-lived Node worker, keyed by
//! `(language, project_root)`. Workers speak NDJSON over stdin/stdout: one
//! request line in, one response line out. A `Mutex` around each worker
//! serialises rayon callers — Node is single-threaded anyway — and the cost
//! that previously dominated the build (a fresh `Command::new("node")` plus
//! `require('sass')` per component) collapses to a single startup amortised
//! across every styled component in the build.
//!
//! The matching npm package (`sass`, `less`, or `stylus`) must be installed
//! in the project; if it is missing we surface a clear [`NgcError::StyleError`]
//! so the user can `npm install` it. If a worker dies mid-build (parser
//! crashed, OOM, etc.) we surface the captured stderr as a `StyleError` and
//! drop the entry so the next request spawns a fresh worker.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use ngc_diagnostics::{NgcError, NgcResult};

/// Style source language, derived from a file extension or from
/// `inlineStyleLanguage` in `angular.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StyleLanguage {
    /// Plain CSS — passthrough (no subprocess).
    #[default]
    Css,
    /// SCSS (`sass` package, default syntax).
    Scss,
    /// Sass indented syntax (`sass` package, `syntax: 'indented'`).
    Sass,
    /// Less (`less` package).
    Less,
    /// Stylus (`stylus` package).
    Stylus,
}

impl StyleLanguage {
    /// Map a file extension (no leading dot) to a language. Unknown extensions
    /// fall back to [`StyleLanguage::Css`].
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "scss" => StyleLanguage::Scss,
            "sass" => StyleLanguage::Sass,
            "less" => StyleLanguage::Less,
            "styl" | "stylus" => StyleLanguage::Stylus,
            _ => StyleLanguage::Css,
        }
    }

    /// npm package that provides the preprocessor, or `None` for plain CSS.
    pub fn npm_package(self) -> Option<&'static str> {
        match self {
            StyleLanguage::Css => None,
            StyleLanguage::Scss | StyleLanguage::Sass => Some("sass"),
            StyleLanguage::Less => Some("less"),
            StyleLanguage::Stylus => Some("stylus"),
        }
    }

    /// Human-readable name used in diagnostic messages.
    pub fn as_str(self) -> &'static str {
        match self {
            StyleLanguage::Css => "css",
            StyleLanguage::Scss => "scss",
            StyleLanguage::Sass => "sass",
            StyleLanguage::Less => "less",
            StyleLanguage::Stylus => "stylus",
        }
    }

    fn worker_script(self) -> &'static str {
        match self {
            StyleLanguage::Scss => SCSS_WORKER,
            StyleLanguage::Sass => SASS_INDENTED_WORKER,
            StyleLanguage::Less => LESS_WORKER,
            StyleLanguage::Stylus => STYLUS_WORKER,
            StyleLanguage::Css => "",
        }
    }
}

/// Compile `content` into plain CSS using `language`'s preprocessor.
///
/// `project_root` is used to locate the npm package in `node_modules` and as
/// the worker's working directory (so relative `@use`/`@import` resolve
/// against the project). `source_path` is attached to diagnostics.
///
/// Plain CSS is returned unchanged — no subprocess is involved.
pub fn preprocess_style(
    content: &str,
    language: StyleLanguage,
    project_root: &Path,
    source_path: &Path,
) -> NgcResult<String> {
    if language == StyleLanguage::Css {
        return Ok(content.to_string());
    }
    let pkg = language
        .npm_package()
        .expect("non-css language has a package");
    let pkg_dir = project_root.join("node_modules").join(pkg);
    if !pkg_dir.is_dir() {
        return Err(NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!(
                "cannot preprocess {} styles: the `{pkg}` npm package is not installed in {}. \
                 Run `npm install --save-dev {pkg}` and retry.",
                language.as_str(),
                project_root.display()
            ),
        });
    }

    let slot = worker_slot(language, project_root);
    let mut guard = slot
        .lock()
        .map_err(|_| style_error(source_path, language, "worker mutex poisoned"))?;

    if guard.is_none() {
        *guard = Some(StyleWorker::spawn(language, project_root, source_path)?);
    }
    let worker = guard.as_mut().expect("just inserted");

    match worker.request(content) {
        Ok(WorkerOutcome::Css(css)) => Ok(css),
        Ok(WorkerOutcome::CompileError(message)) => Err(NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!(
                "{} preprocessing failed: {}",
                language.as_str(),
                message.trim()
            ),
        }),
        Err(fatal) => {
            *guard = None;
            Err(NgcError::StyleError {
                path: source_path.to_path_buf(),
                message: format!(
                    "{} preprocessor subprocess crashed: {}",
                    language.as_str(),
                    fatal
                ),
            })
        }
    }
}

/// Convenience: read a style file from disk and preprocess it.
pub fn preprocess_file(path: &Path, project_root: &Path) -> NgcResult<String> {
    let content = std::fs::read_to_string(path).map_err(|e| NgcError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let language = StyleLanguage::from_extension(ext);
    preprocess_style(&content, language, project_root, path)
}

/// Return type shared by component preprocessing helpers.
#[derive(Debug, Clone, Default)]
pub struct ComponentStyles {
    /// Compiled CSS strings, one per entry in the original `styles[]` /
    /// `styleUrls[]` declaration, preserving source order.
    pub compiled_css: Vec<String>,
    /// Absolute paths of `styleUrl`/`styleUrls` entries that were resolved
    /// from disk. Useful for surface-level reporting.
    #[allow(dead_code)]
    pub resolved_urls: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Worker registry
// ---------------------------------------------------------------------------

type WorkerSlot = Arc<Mutex<Option<StyleWorker>>>;

fn worker_slot(language: StyleLanguage, project_root: &Path) -> WorkerSlot {
    static REGISTRY: OnceLock<Mutex<HashMap<(StyleLanguage, PathBuf), WorkerSlot>>> =
        OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (language, project_root.to_path_buf());
    let mut map = registry.lock().expect("worker registry poisoned");
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

fn style_error(source_path: &Path, language: StyleLanguage, what: &str) -> NgcError {
    NgcError::StyleError {
        path: source_path.to_path_buf(),
        message: format!("{}: {}", language.as_str(), what),
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

enum WorkerOutcome {
    Css(String),
    CompileError(String),
}

struct StyleWorker {
    /// Stays alive as long as the worker. Drop kills the process.
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_buf: Arc<Mutex<String>>,
    next_id: u64,
    language: StyleLanguage,
}

impl StyleWorker {
    fn spawn(language: StyleLanguage, project_root: &Path, source_path: &Path) -> NgcResult<Self> {
        let mut child = Command::new("node")
            .arg("-e")
            .arg(language.worker_script())
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| NgcError::StyleError {
                path: source_path.to_path_buf(),
                message: format!(
                    "could not run node for {} preprocessing: {e}",
                    language.as_str()
                ),
            })?;

        let stdin = child.stdin.take().ok_or_else(|| NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!("could not open stdin for {} subprocess", language.as_str()),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!("could not open stdout for {} subprocess", language.as_str()),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!("could not open stderr for {} subprocess", language.as_str()),
        })?;

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf_thr = Arc::clone(&stderr_buf);
        thread::spawn(move || {
            let mut reader = stderr;
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            if let Ok(mut guard) = stderr_buf_thr.lock() {
                guard.push_str(&String::from_utf8_lossy(&buf));
            }
        });

        Ok(StyleWorker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_buf,
            next_id: 0,
            language,
        })
    }

    fn request(&mut self, content: &str) -> Result<WorkerOutcome, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let payload = serde_json::json!({ "id": id, "input": content });
        let line = serde_json::to_string(&payload)
            .map_err(|e| format!("could not encode request: {e}"))?;

        if let Err(e) = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
        {
            return Err(format!(
                "could not write request: {e} ({})",
                self.drain_stderr()
            ));
        }

        let mut response = String::new();
        let n = self
            .stdout
            .read_line(&mut response)
            .map_err(|e| format!("could not read response: {e} ({})", self.drain_stderr()))?;
        if n == 0 {
            return Err(format!(
                "subprocess exited before reply ({})",
                self.drain_stderr()
            ));
        }

        let value: serde_json::Value = serde_json::from_str(response.trim_end())
            .map_err(|e| format!("malformed response: {e}; payload: {response}"))?;
        let resp_id = value.get("id").and_then(|v| v.as_u64());
        if resp_id != Some(id) {
            return Err(format!(
                "id mismatch: expected {id}, got {resp_id:?} (payload: {response})"
            ));
        }
        let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            let err = value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error message)")
                .to_string();
            return Ok(WorkerOutcome::CompileError(err));
        }
        let css = value
            .get("css")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(WorkerOutcome::Css(css))
    }

    /// Best-effort capture of any stderr the worker emitted before dying. The
    /// drain thread reads asynchronously, so we sleep briefly to give it a
    /// chance to flush after an EOF on stdout.
    fn drain_stderr(&self) -> String {
        thread::sleep(Duration::from_millis(20));
        let s = self
            .stderr_buf
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let trimmed = s.trim();
        if trimmed.is_empty() {
            format!("no stderr from {} worker", self.language.as_str())
        } else {
            trimmed.to_string()
        }
    }
}

impl Drop for StyleWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Worker scripts (NDJSON: one request line in, one response line out)
// ---------------------------------------------------------------------------
//
// Request:  {"id": <u64>, "input": "<css source>"}
// Response: {"id": <u64>, "ok": true,  "css":   "<compiled css>"}
//        or {"id": <u64>, "ok": false, "error": "<message>"}

const SCSS_WORKER: &str = r#"
const sass = require('sass');
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
    let req;
    try { req = JSON.parse(line); }
    catch (e) {
        process.stdout.write(JSON.stringify({ id: -1, ok: false, error: 'invalid request: ' + (e && e.message ? e.message : String(e)) }) + '\n');
        return;
    }
    try {
        const out = sass.compileString(req.input);
        process.stdout.write(JSON.stringify({ id: req.id, ok: true, css: out.css }) + '\n');
    } catch (err) {
        const msg = (err && err.message) ? err.message : String(err);
        process.stdout.write(JSON.stringify({ id: req.id, ok: false, error: msg }) + '\n');
    }
});
rl.on('close', () => process.exit(0));
"#;

const SASS_INDENTED_WORKER: &str = r#"
const sass = require('sass');
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
    let req;
    try { req = JSON.parse(line); }
    catch (e) {
        process.stdout.write(JSON.stringify({ id: -1, ok: false, error: 'invalid request: ' + (e && e.message ? e.message : String(e)) }) + '\n');
        return;
    }
    try {
        const out = sass.compileString(req.input, { syntax: 'indented' });
        process.stdout.write(JSON.stringify({ id: req.id, ok: true, css: out.css }) + '\n');
    } catch (err) {
        const msg = (err && err.message) ? err.message : String(err);
        process.stdout.write(JSON.stringify({ id: req.id, ok: false, error: msg }) + '\n');
    }
});
rl.on('close', () => process.exit(0));
"#;

const LESS_WORKER: &str = r#"
const less = require('less');
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
    let req;
    try { req = JSON.parse(line); }
    catch (e) {
        process.stdout.write(JSON.stringify({ id: -1, ok: false, error: 'invalid request: ' + (e && e.message ? e.message : String(e)) }) + '\n');
        return;
    }
    less.render(req.input).then((out) => {
        process.stdout.write(JSON.stringify({ id: req.id, ok: true, css: out.css }) + '\n');
    }).catch((err) => {
        const msg = (err && err.message) ? err.message : String(err);
        process.stdout.write(JSON.stringify({ id: req.id, ok: false, error: msg }) + '\n');
    });
});
rl.on('close', () => process.exit(0));
"#;

const STYLUS_WORKER: &str = r#"
const stylus = require('stylus');
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
    let req;
    try { req = JSON.parse(line); }
    catch (e) {
        process.stdout.write(JSON.stringify({ id: -1, ok: false, error: 'invalid request: ' + (e && e.message ? e.message : String(e)) }) + '\n');
        return;
    }
    stylus.render(req.input, (err, css) => {
        if (err) {
            const msg = (err && err.message) ? err.message : String(err);
            process.stdout.write(JSON.stringify({ id: req.id, ok: false, error: msg }) + '\n');
            return;
        }
        process.stdout.write(JSON.stringify({ id: req.id, ok: true, css: css }) + '\n');
    });
});
rl.on('close', () => process.exit(0));
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn css_is_passthrough() {
        let out = preprocess_style(
            ".a { color: red; }",
            StyleLanguage::Css,
            Path::new("/tmp"),
            Path::new("inline.css"),
        )
        .unwrap();
        assert_eq!(out, ".a { color: red; }");
    }

    #[test]
    fn missing_package_yields_style_error() {
        let tmp = tempfile_dir();
        let err = preprocess_style(
            "$x: 1;\n.a { width: $x; }",
            StyleLanguage::Scss,
            &tmp,
            Path::new("inline.scss"),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("sass"), "expected sass in message: {msg}");
        assert!(msg.contains("npm install"), "expected install hint: {msg}");
    }

    #[test]
    fn extension_maps_to_language() {
        assert_eq!(StyleLanguage::from_extension("scss"), StyleLanguage::Scss);
        assert_eq!(StyleLanguage::from_extension("SASS"), StyleLanguage::Sass);
        assert_eq!(StyleLanguage::from_extension("less"), StyleLanguage::Less);
        assert_eq!(StyleLanguage::from_extension("styl"), StyleLanguage::Stylus);
        assert_eq!(
            StyleLanguage::from_extension("stylus"),
            StyleLanguage::Stylus
        );
        assert_eq!(StyleLanguage::from_extension("css"), StyleLanguage::Css);
        assert_eq!(StyleLanguage::from_extension("txt"), StyleLanguage::Css);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ngc-preproc-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
