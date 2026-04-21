use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use serde::Deserialize;
use tracing::debug;

/// Raw JSON representation of a tsconfig.json file.
///
/// Fields are all optional because any of them can be inherited via `extends`.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawTsConfig {
    /// Path to a parent tsconfig to inherit from.
    pub extends: Option<String>,
    /// Compiler options.
    pub compiler_options: Option<CompilerOptions>,
    /// Glob patterns for files to include.
    pub include: Option<Vec<String>>,
    /// Glob patterns for files to exclude.
    pub exclude: Option<Vec<String>>,
    /// Explicit file list.
    pub files: Option<Vec<String>>,
}

/// The compilerOptions section of tsconfig.json.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CompilerOptions {
    /// Base URL for resolving non-relative module names.
    pub base_url: Option<String>,
    /// Path alias mappings (e.g., "@app/*" -> ["src/app/*"]).
    pub paths: Option<HashMap<String, Vec<String>>>,
    /// Root directory of input files.
    pub root_dir: Option<String>,
    /// Output directory.
    pub out_dir: Option<String>,
    /// Module resolution strategy.
    pub module_resolution: Option<String>,
}

/// A fully resolved tsconfig with all `extends` chains merged.
#[derive(Debug, Clone)]
pub struct ResolvedTsConfig {
    /// The path to the tsconfig file this was loaded from.
    pub config_path: PathBuf,
    /// Merged compiler options from the full extends chain.
    pub compiler_options: CompilerOptions,
    /// Resolved include patterns.
    pub include: Vec<String>,
    /// Resolved exclude patterns.
    pub exclude: Vec<String>,
    /// Explicit file list.
    pub files: Vec<String>,
}

/// Parse a tsconfig.json and resolve its full `extends` chain.
///
/// Reads the file at `config_path`, follows the `extends` chain to the root,
/// and merges all configurations with child values taking precedence.
pub fn resolve_tsconfig(config_path: &Path) -> NgcResult<ResolvedTsConfig> {
    let canonical = config_path.canonicalize().map_err(|e| NgcError::Io {
        path: config_path.to_path_buf(),
        source: e,
    })?;
    let mut visited = HashSet::new();
    resolve_tsconfig_inner(&canonical, &mut visited)
}

/// Internal recursive resolver that tracks visited paths for cycle detection.
fn resolve_tsconfig_inner(
    config_path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> NgcResult<ResolvedTsConfig> {
    if !visited.insert(config_path.to_path_buf()) {
        return Err(NgcError::TsConfigCircularExtends {
            chain: visited.iter().cloned().collect(),
        });
    }

    debug!(?config_path, "parsing tsconfig");

    let contents = std::fs::read_to_string(config_path).map_err(|e| NgcError::Io {
        path: config_path.to_path_buf(),
        source: e,
    })?;

    let stripped = strip_jsonc_comments(&contents);
    let raw: RawTsConfig =
        serde_json::from_str(&stripped).map_err(|e| NgcError::TsConfigParse {
            path: config_path.to_path_buf(),
            source: e,
        })?;

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    // If extends is set, resolve the parent first
    let base = if let Some(ref extends_path) = raw.extends {
        let mut parent_path = config_dir.join(extends_path);
        if parent_path.extension().is_none() {
            parent_path.set_extension("json");
        }
        let parent_canonical =
            parent_path
                .canonicalize()
                .map_err(|_| NgcError::TsConfigExtendsNotFound {
                    path: parent_path.clone(),
                })?;
        Some(resolve_tsconfig_inner(&parent_canonical, visited)?)
    } else {
        None
    };

    // Merge: child overrides parent
    let compiler_options = merge_compiler_options(
        base.as_ref().map(|b| &b.compiler_options),
        raw.compiler_options.as_ref(),
    );

    let include = raw
        .include
        .or(base.as_ref().map(|b| b.include.clone()))
        .unwrap_or_default();

    let exclude = raw
        .exclude
        .or(base.as_ref().map(|b| b.exclude.clone()))
        .unwrap_or_default();

    let files = raw
        .files
        .or(base.as_ref().map(|b| b.files.clone()))
        .unwrap_or_default();

    Ok(ResolvedTsConfig {
        config_path: config_path.to_path_buf(),
        compiler_options,
        include,
        exclude,
        files,
    })
}

/// Strip `//` line comments and `/* */` block comments from JSONC input.
///
/// Preserves comment-like sequences inside JSON string literals.
/// tsconfig.json files are JSONC (JSON with Comments) by convention.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'"' => {
                // String literal — copy verbatim until closing quote
                out.push('"');
                i += 1;
                while i < len {
                    if bytes[i] == b'\\' && i + 1 < len {
                        out.push(bytes[i] as char);
                        out.push(bytes[i + 1] as char);
                        i += 2;
                    } else if bytes[i] == b'"' {
                        out.push('"');
                        i += 1;
                        break;
                    } else {
                        out.push(bytes[i] as char);
                        i += 1;
                    }
                }
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                // Line comment — skip to end of line
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                // Block comment — skip to */
                i += 2;
                while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < len {
                    i += 2; // skip */
                }
            }
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }

    out
}

/// Merge compiler options from parent and child, with child taking precedence.
fn merge_compiler_options(
    parent: Option<&CompilerOptions>,
    child: Option<&CompilerOptions>,
) -> CompilerOptions {
    match (parent, child) {
        (None, None) => CompilerOptions::default(),
        (Some(p), None) => p.clone(),
        (None, Some(c)) => c.clone(),
        (Some(p), Some(c)) => CompilerOptions {
            base_url: c.base_url.clone().or(p.base_url.clone()),
            paths: c.paths.clone().or(p.paths.clone()),
            root_dir: c.root_dir.clone().or(p.root_dir.clone()),
            out_dir: c.out_dir.clone().or(p.out_dir.clone()),
            module_resolution: c.module_resolution.clone().or(p.module_resolution.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_circular_extends_detection() {
        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.json");
        let b_path = dir.path().join("b.json");

        fs::write(&a_path, r#"{ "extends": "./b.json" }"#).unwrap();
        fs::write(&b_path, r#"{ "extends": "./a.json" }"#).unwrap();

        let result = resolve_tsconfig(&a_path);
        assert!(result.is_err());
        match result.unwrap_err() {
            NgcError::TsConfigCircularExtends { chain } => {
                assert!(chain.len() >= 2);
            }
            other => panic!("expected TsConfigCircularExtends, got: {other}"),
        }
    }

    #[test]
    fn test_extends_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("tsconfig.json");
        fs::write(&config_path, r#"{ "extends": "./nonexistent.json" }"#).unwrap();

        let result = resolve_tsconfig(&config_path);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            NgcError::TsConfigExtendsNotFound { .. }
        ));
    }

    #[test]
    fn test_jsonc_with_line_comments() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("tsconfig.json");
        fs::write(
            &config_path,
            r#"// this is a comment
{
  // another comment
  "compilerOptions": {
    "baseUrl": "."
  }
}"#,
        )
        .unwrap();

        let config = resolve_tsconfig(&config_path).unwrap();
        assert_eq!(config.compiler_options.base_url.as_deref(), Some("."));
    }

    #[test]
    fn test_jsonc_with_block_comments() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("tsconfig.json");
        fs::write(
            &config_path,
            r#"/* To learn more about Typescript configuration file: https://www.typescriptlang.org/docs/handbook/tsconfig-json.html. */
/* To learn more about Angular compiler options: https://angular.dev/reference/configs/angular-compiler-options. */
{
  "compilerOptions": {
    "baseUrl": "."
  }
}"#,
        )
        .unwrap();

        let config = resolve_tsconfig(&config_path).unwrap();
        assert_eq!(config.compiler_options.base_url.as_deref(), Some("."));
    }

    #[test]
    fn test_jsonc_preserves_strings_with_slashes() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("tsconfig.json");
        fs::write(
            &config_path,
            r#"{
  "compilerOptions": {
    "baseUrl": "https://example.com"
  }
}"#,
        )
        .unwrap();

        let config = resolve_tsconfig(&config_path).unwrap();
        assert_eq!(
            config.compiler_options.base_url.as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn test_empty_tsconfig() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("tsconfig.json");
        fs::write(&config_path, "{}").unwrap();

        let config = resolve_tsconfig(&config_path).unwrap();
        assert!(config.include.is_empty());
        assert!(config.files.is_empty());
        assert!(config.compiler_options.paths.is_none());
    }
}
