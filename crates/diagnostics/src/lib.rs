use std::path::PathBuf;
use thiserror::Error;

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
    #[error("parse error in {path}: {message}")]
    ParseError {
        /// The path to the file that failed to parse.
        path: PathBuf,
        /// The error message from the parser.
        message: String,
    },

    /// A TypeScript transform failed.
    #[error("transform error in {path}: {message}")]
    TransformError {
        /// The path to the file that failed to transform.
        path: PathBuf,
        /// The error message from the transformer.
        message: String,
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
    #[error("template parse error in {path}: {message}")]
    TemplateParseError {
        /// The path to the file containing the template.
        path: PathBuf,
        /// The error message from the template parser.
        message: String,
    },

    /// Angular template compilation (Ivy codegen) failed.
    #[error("template compile error in {path}: {message}")]
    TemplateCompileError {
        /// The path to the file that failed to compile.
        path: PathBuf,
        /// The error message from the compiler.
        message: String,
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
    #[error("linker error in {path}: {message}")]
    LinkerError {
        /// The path to the file that failed to link.
        path: PathBuf,
        /// Description of what went wrong.
        message: String,
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
