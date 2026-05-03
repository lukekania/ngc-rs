use std::path::PathBuf;
use thiserror::Error;

/// Convert a 0-based UTF-8 byte offset into `source` to a 1-based
/// (line, column) pair.
///
/// Columns count by Unicode code points (`char`s), not bytes — this matches
/// how editors locate cursors on multibyte characters. If `offset` lies past
/// the end of `source`, returns the position of the last byte in the file.
///
/// Used by build-pipeline crates to attach precise error locations to
/// `NgcError` variants that carry `line` / `column` fields (e.g.
/// [`NgcError::TemplateCompileError`]). The overlay client surfaces those
/// fields verbatim.
pub fn byte_offset_to_line_col(source: &str, offset: u32) -> (u32, u32) {
    let target = (offset as usize).min(source.len());
    let mut line: u32 = 1;
    let mut col: u32 = 1;
    let mut consumed = 0usize;
    for ch in source.chars() {
        if consumed >= target {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
        consumed += ch.len_utf8();
    }
    (line, col)
}

/// The unified error type for all ngc-rs operations.
#[derive(Debug, Error)]
pub enum NgcError {
    /// An IO operation failed.
    #[error("IO error at {path}: {source}")]
    Io {
        /// The path where the IO error occurred.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },

    /// A tsconfig.json file could not be parsed as valid JSON.
    #[error("failed to parse tsconfig at {path}: {source}")]
    TsConfigParse {
        /// The path to the invalid tsconfig file.
        path: PathBuf,
        /// The underlying JSON parse error.
        source: serde_json::Error,
    },

    /// A tsconfig.json `extends` field references a file that does not exist.
    #[error("tsconfig extends target not found: {path}")]
    TsConfigExtendsNotFound {
        /// The path that was referenced but not found.
        path: PathBuf,
    },

    /// A circular `extends` chain was detected in tsconfig files.
    #[error("circular tsconfig extends chain detected: {chain:?}")]
    TsConfigCircularExtends {
        /// The chain of paths forming the cycle.
        chain: Vec<PathBuf>,
    },

    /// An import in a source file references a file that cannot be resolved.
    #[error("unresolved import {specifier:?} in {from_file}")]
    UnresolvedImport {
        /// The raw import specifier string.
        specifier: String,
        /// The file containing the unresolved import.
        from_file: PathBuf,
    },

    /// A path alias pattern is malformed.
    #[error("invalid path alias pattern: {pattern}")]
    InvalidPathAlias {
        /// The malformed pattern.
        pattern: String,
    },

    /// A TypeScript file could not be parsed.
    #[error("parse error in {path}{}: {message}", fmt_loc(*line, *column))]
    ParseError {
        /// The path to the file that failed to parse.
        path: PathBuf,
        /// The error message from the parser.
        message: String,
        /// 1-based line number of the first parser error, when available.
        line: Option<u32>,
        /// 1-based column number of the first parser error, when available.
        column: Option<u32>,
    },

    /// A TypeScript transform failed.
    #[error("transform error in {path}{}: {message}", fmt_loc(*line, *column))]
    TransformError {
        /// The path to the file that failed to transform.
        path: PathBuf,
        /// The error message from the transformer.
        message: String,
        /// 1-based line number of the first transformer error, when available.
        line: Option<u32>,
        /// 1-based column number of the first transformer error, when available.
        column: Option<u32>,
    },

    /// A bundling operation failed.
    #[error("bundle error: {message}")]
    BundleError {
        /// Description of what went wrong during bundling.
        message: String,
    },

    /// A circular dependency was detected in the module graph.
    #[error("circular dependency detected: {cycle:?}")]
    CircularDependency {
        /// The file paths forming the dependency cycle.
        cycle: Vec<PathBuf>,
    },

    /// An Angular template could not be parsed.
    #[error("template parse error in {path}{}: {message}", fmt_loc(*line, *column))]
    TemplateParseError {
        /// The path to the file containing the template.
        path: PathBuf,
        /// The error message from the template parser.
        message: String,
        /// 1-based line number of the first parser error, when available.
        line: Option<u32>,
        /// 1-based column number of the first parser error, when available.
        column: Option<u32>,
    },

    /// Angular template compilation (Ivy codegen) failed.
    #[error("template compile error in {path}{}: {message}", fmt_loc(*line, *column))]
    TemplateCompileError {
        /// The path to the file that failed to compile.
        path: PathBuf,
        /// The error message from the compiler.
        message: String,
        /// 1-based line number where compilation failed, when available.
        line: Option<u32>,
        /// 1-based column number where compilation failed, when available.
        column: Option<u32>,
    },

    /// An angular.json file could not be parsed.
    #[error("failed to parse angular.json at {path}: {source}")]
    AngularJsonParse {
        /// The path to the invalid angular.json file.
        path: PathBuf,
        /// The underlying JSON parse error.
        source: serde_json::Error,
    },

    /// A referenced project was not found in angular.json.
    #[error("project {name:?} not found in angular.json at {path}")]
    ProjectNotFound {
        /// The project name that was not found.
        name: String,
        /// The path to the angular.json file.
        path: PathBuf,
    },

    /// An asset path or pattern is invalid.
    #[error("asset error for {path}: {message}")]
    AssetError {
        /// The problematic asset path.
        path: PathBuf,
        /// Description of what went wrong.
        message: String,
    },

    /// A style file could not be processed.
    #[error("style error for {path}: {message}")]
    StyleError {
        /// The path to the problematic style file.
        path: PathBuf,
        /// Description of what went wrong.
        message: String,
    },

    /// JSON serialization failed.
    #[error("JSON output error: {message}")]
    JsonOutputError {
        /// Description of what went wrong.
        message: String,
    },

    /// A code splitting / chunk graph error occurred.
    #[error("chunk error: {message}")]
    ChunkError {
        /// Description of what went wrong during chunk graph construction.
        message: String,
    },

    /// Source map generation failed.
    #[error("source map error for {path}: {message}")]
    SourceMapError {
        /// The path to the file that caused the source map error.
        path: PathBuf,
        /// Description of what went wrong.
        message: String,
    },

    /// Minification failed.
    #[error("minification error for {path}: {message}")]
    MinifyError {
        /// The path to the file that failed to minify.
        path: PathBuf,
        /// Description of what went wrong.
        message: String,
    },

    /// An npm package could not be resolved.
    #[error("npm resolution error for {specifier}: {message}")]
    NpmResolutionError {
        /// The bare module specifier that failed to resolve.
        specifier: String,
        /// Description of what went wrong.
        message: String,
    },

    /// The Angular linker failed to process a partially compiled file.
    #[error("linker error in {path}{}: {message}", fmt_loc(*line, *column))]
    LinkerError {
        /// The path to the file that failed to link.
        path: PathBuf,
        /// Description of what went wrong.
        message: String,
        /// 1-based line number where the linker failure occurred, when available.
        line: Option<u32>,
        /// 1-based column number where the linker failure occurred, when available.
        column: Option<u32>,
    },

    /// A user-facing configuration error not tied to a specific file
    /// (e.g. a CLI flag that requires a corresponding `angular.json` block).
    #[error("configuration error: {message}")]
    ConfigError {
        /// Description of the misconfiguration.
        message: String,
    },

    /// A filesystem watcher error (notify backend failure, missing path, etc.).
    #[error("watch error: {message}")]
    WatchError {
        /// Description of what went wrong while watching files.
        message: String,
    },

    /// The dev server failed to start, bind, or otherwise serve requests.
    #[error("dev server error: {message}")]
    ServeError {
        /// Description of what went wrong.
        message: String,
    },
}

/// A type alias for Results using NgcError.
pub type NgcResult<T> = Result<T, NgcError>;

impl NgcError {
    /// Returns the source file associated with this error, when one exists.
    /// Variants that describe a file or location problem expose their `path`
    /// field; variants that describe a stage-level failure (e.g.
    /// [`NgcError::BundleError`], [`NgcError::ConfigError`]) return `None`.
    /// The architect builder shim consumes this to populate `Diagnostic.file`.
    pub fn file(&self) -> Option<&std::path::Path> {
        match self {
            NgcError::Io { path, .. }
            | NgcError::TsConfigParse { path, .. }
            | NgcError::TsConfigExtendsNotFound { path }
            | NgcError::ParseError { path, .. }
            | NgcError::TransformError { path, .. }
            | NgcError::TemplateParseError { path, .. }
            | NgcError::TemplateCompileError { path, .. }
            | NgcError::AngularJsonParse { path, .. }
            | NgcError::ProjectNotFound { path, .. }
            | NgcError::AssetError { path, .. }
            | NgcError::StyleError { path, .. }
            | NgcError::SourceMapError { path, .. }
            | NgcError::MinifyError { path, .. }
            | NgcError::LinkerError { path, .. } => Some(path),
            NgcError::UnresolvedImport { from_file, .. } => Some(from_file),
            NgcError::TsConfigCircularExtends { chain } => chain.first().map(PathBuf::as_path),
            NgcError::CircularDependency { cycle } => cycle.first().map(PathBuf::as_path),
            NgcError::InvalidPathAlias { .. }
            | NgcError::BundleError { .. }
            | NgcError::JsonOutputError { .. }
            | NgcError::ChunkError { .. }
            | NgcError::NpmResolutionError { .. }
            | NgcError::ConfigError { .. }
            | NgcError::WatchError { .. }
            | NgcError::ServeError { .. } => None,
        }
    }

    /// Returns the 1-based source line number associated with this error,
    /// when one is available. Only variants that carry parser/compiler
    /// location data populate this.
    pub fn line(&self) -> Option<u32> {
        match self {
            NgcError::ParseError { line, .. }
            | NgcError::TransformError { line, .. }
            | NgcError::TemplateParseError { line, .. }
            | NgcError::TemplateCompileError { line, .. }
            | NgcError::LinkerError { line, .. } => *line,
            _ => None,
        }
    }

    /// Returns the 1-based source column number associated with this error,
    /// when one is available. Only variants that carry parser/compiler
    /// location data populate this.
    pub fn column(&self) -> Option<u32> {
        match self {
            NgcError::ParseError { column, .. }
            | NgcError::TransformError { column, .. }
            | NgcError::TemplateParseError { column, .. }
            | NgcError::TemplateCompileError { column, .. }
            | NgcError::LinkerError { column, .. } => *column,
            _ => None,
        }
    }
}

/// Format an optional `(line, column)` pair as `:line:col` (or `:line` when
/// only the line is known) for embedding in `Display` output. Returns an
/// empty string when neither is known.
fn fmt_loc(line: Option<u32>, column: Option<u32>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(":{l}:{c}"),
        (Some(l), None) => format!(":{l}"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offset_at_start_returns_line_one_col_one() {
        assert_eq!(byte_offset_to_line_col("hello\nworld", 0), (1, 1));
    }

    #[test]
    fn byte_offset_advances_columns_within_a_line() {
        assert_eq!(byte_offset_to_line_col("hello", 3), (1, 4));
    }

    #[test]
    fn byte_offset_advances_lines_after_newline() {
        assert_eq!(byte_offset_to_line_col("a\nbcd", 2), (2, 1));
        assert_eq!(byte_offset_to_line_col("a\nbcd", 4), (2, 3));
    }

    #[test]
    fn byte_offset_counts_columns_by_chars_not_bytes() {
        // 'é' is two UTF-8 bytes; column should advance by 1.
        let s = "é-x";
        assert_eq!(byte_offset_to_line_col(s, 0), (1, 1));
        assert_eq!(byte_offset_to_line_col(s, 2), (1, 2));
    }

    #[test]
    fn byte_offset_past_end_clamps_to_last_position() {
        let s = "abc";
        let (l, c) = byte_offset_to_line_col(s, 999);
        assert_eq!(l, 1);
        assert_eq!(c, 4);
    }

    #[test]
    fn fmt_loc_renders_line_col_when_both_present() {
        assert_eq!(fmt_loc(Some(12), Some(3)), ":12:3");
    }

    #[test]
    fn fmt_loc_renders_just_line_when_no_column() {
        assert_eq!(fmt_loc(Some(12), None), ":12");
    }

    #[test]
    fn fmt_loc_renders_empty_when_missing() {
        assert_eq!(fmt_loc(None, None), "");
        assert_eq!(fmt_loc(None, Some(5)), "");
    }

    #[test]
    fn file_accessor_returns_path_for_file_variants() {
        let e = NgcError::ParseError {
            path: PathBuf::from("/x/y.ts"),
            message: "boom".into(),
            line: Some(3),
            column: Some(7),
        };
        assert_eq!(e.file(), Some(std::path::Path::new("/x/y.ts")));
        assert_eq!(e.line(), Some(3));
        assert_eq!(e.column(), Some(7));
    }

    #[test]
    fn file_accessor_returns_from_file_for_unresolved_import() {
        let e = NgcError::UnresolvedImport {
            specifier: "./missing".into(),
            from_file: PathBuf::from("/x/main.ts"),
        };
        assert_eq!(e.file(), Some(std::path::Path::new("/x/main.ts")));
        assert_eq!(e.line(), None);
        assert_eq!(e.column(), None);
    }

    #[test]
    fn file_accessor_returns_none_for_stage_level_variants() {
        let e = NgcError::BundleError {
            message: "boom".into(),
        };
        assert_eq!(e.file(), None);
        assert_eq!(e.line(), None);
        assert_eq!(e.column(), None);
    }

    #[test]
    fn file_accessor_returns_first_path_in_chain() {
        let chain = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let e = NgcError::TsConfigCircularExtends {
            chain: chain.clone(),
        };
        assert_eq!(e.file(), Some(chain[0].as_path()));
    }
}
