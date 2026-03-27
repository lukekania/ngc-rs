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
}

/// A type alias for Results using NgcError.
pub type NgcResult<T> = Result<T, NgcError>;
