//! Component-style preprocessor harness for SCSS / Sass / Less / Stylus.
//!
//! Mirrors the PostCSS/Tailwind subprocess pattern used for global styles:
//! a short Node script is spawned, the raw source is piped to stdin, and the
//! compiled CSS is read back from stdout. The matching npm package (`sass`,
//! `less`, or `stylus`) must be installed in the project; if it is missing we
//! surface a clear [`NgcError::StyleError`] so the user can `npm install` it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ngc_diagnostics::{NgcError, NgcResult};

/// Style source language, derived from a file extension or from
/// `inlineStyleLanguage` in `angular.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    fn node_script(self) -> &'static str {
        match self {
            StyleLanguage::Scss => SCSS_SCRIPT,
            StyleLanguage::Sass => SASS_INDENTED_SCRIPT,
            StyleLanguage::Less => LESS_SCRIPT,
            StyleLanguage::Stylus => STYLUS_SCRIPT,
            StyleLanguage::Css => "",
        }
    }
}

/// Compile `content` into plain CSS using `language`'s preprocessor.
///
/// `project_root` is used to locate the npm package in `node_modules` and as
/// the subprocess's working directory (so relative `@use`/`@import` resolve
/// against the project). `source_path` is attached to diagnostics.
///
/// Plain CSS is returned unchanged — no subprocess is spawned.
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

    let mut child = Command::new("node")
        .arg("-e")
        .arg(language.node_script())
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

    {
        let stdin = child.stdin.as_mut().ok_or_else(|| NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!("could not open stdin for {} subprocess", language.as_str()),
        })?;
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| NgcError::StyleError {
                path: source_path.to_path_buf(),
                message: format!(
                    "could not write to {} subprocess stdin: {e}",
                    language.as_str()
                ),
            })?;
    }

    let output = child.wait_with_output().map_err(|e| NgcError::StyleError {
        path: source_path.to_path_buf(),
        message: format!("failed to await {} subprocess: {e}", language.as_str()),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NgcError::StyleError {
            path: source_path.to_path_buf(),
            message: format!(
                "{} preprocessing failed: {}",
                language.as_str(),
                stderr.trim()
            ),
        });
    }
    String::from_utf8(output.stdout).map_err(|e| NgcError::StyleError {
        path: source_path.to_path_buf(),
        message: format!(
            "{} preprocessor output was not valid UTF-8: {e}",
            language.as_str()
        ),
    })
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

const SCSS_SCRIPT: &str = r#"
const sass = require('sass');
const chunks = [];
process.stdin.on('data', c => chunks.push(c));
process.stdin.on('end', () => {
    try {
        const input = Buffer.concat(chunks).toString('utf8');
        const out = sass.compileString(input);
        process.stdout.write(out.css);
    } catch (err) {
        console.error(err && err.message ? err.message : String(err));
        process.exit(1);
    }
});
"#;

const SASS_INDENTED_SCRIPT: &str = r#"
const sass = require('sass');
const chunks = [];
process.stdin.on('data', c => chunks.push(c));
process.stdin.on('end', () => {
    try {
        const input = Buffer.concat(chunks).toString('utf8');
        const out = sass.compileString(input, { syntax: 'indented' });
        process.stdout.write(out.css);
    } catch (err) {
        console.error(err && err.message ? err.message : String(err));
        process.exit(1);
    }
});
"#;

const LESS_SCRIPT: &str = r#"
const less = require('less');
const chunks = [];
process.stdin.on('data', c => chunks.push(c));
process.stdin.on('end', () => {
    const input = Buffer.concat(chunks).toString('utf8');
    less.render(input).then(out => {
        process.stdout.write(out.css);
    }).catch(err => {
        console.error(err && err.message ? err.message : String(err));
        process.exit(1);
    });
});
"#;

const STYLUS_SCRIPT: &str = r#"
const stylus = require('stylus');
const chunks = [];
process.stdin.on('data', c => chunks.push(c));
process.stdin.on('end', () => {
    const input = Buffer.concat(chunks).toString('utf8');
    stylus.render(input, (err, css) => {
        if (err) {
            console.error(err && err.message ? err.message : String(err));
            process.exit(1);
        }
        process.stdout.write(css);
    });
});
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
