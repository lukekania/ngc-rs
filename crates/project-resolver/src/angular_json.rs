//! Parser for Angular `angular.json` workspace configuration files.
//!
//! Resolves build options including output path, styles, assets, polyfills,
//! and file replacements from the Angular 17+ `angular.json` format.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use serde::Deserialize;
use tracing::debug;

// ---------------------------------------------------------------------------
// Raw deserialization types (match angular.json JSON structure)
// ---------------------------------------------------------------------------

/// Top-level angular.json structure.
#[derive(Debug, Deserialize)]
pub struct RawAngularJson {
    /// Map of project names to project definitions.
    pub projects: HashMap<String, RawProject>,
}

/// A project definition in angular.json.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawProject {
    /// Root directory of the project relative to workspace root.
    pub root: Option<String>,
    /// Source root directory relative to workspace root.
    pub source_root: Option<String>,
    /// Architect targets (build, serve, etc.).
    pub architect: Option<RawArchitect>,
}

/// Architect section containing build targets.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct RawArchitect {
    /// Build target configuration.
    pub build: Option<RawBuildTarget>,
}

/// A build target with default options and named configurations.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawBuildTarget {
    /// Default build options.
    pub options: Option<RawBuildOptions>,
    /// Named configurations (e.g. "production", "development").
    pub configurations: Option<HashMap<String, RawBuildConfiguration>>,
    /// Default configuration name used when none is specified.
    pub default_configuration: Option<String>,
}

/// Build options from angular.json `architect.build.options`.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawBuildOptions {
    /// Output path (string or object with base/browser/server/media).
    pub output_path: Option<RawOutputPath>,
    /// Path to the index HTML file.
    pub index: Option<RawIndex>,
    /// Browser entry point (Angular 17+ field).
    pub browser: Option<String>,
    /// Main entry point (Angular <17 fallback).
    pub main: Option<String>,
    /// Polyfill entries (e.g. `["zone.js"]`).
    pub polyfills: Option<Vec<String>>,
    /// Path to the TypeScript configuration file.
    pub ts_config: Option<String>,
    /// Style file entries.
    pub styles: Option<Vec<RawStyleEntry>>,
    /// Asset entries.
    pub assets: Option<Vec<RawAssetEntry>>,
}

/// Output path can be a simple string or an object for SSR setups.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawOutputPath {
    /// Simple string path (e.g. `"dist/my-app"`).
    Simple(String),
    /// Object form with per-target paths.
    Object {
        /// Base output directory.
        base: Option<String>,
        /// Browser output subdirectory.
        browser: Option<String>,
    },
}

/// Index file reference: string or `{ input, output }` object.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawIndex {
    /// Simple string path (e.g. `"src/index.html"`).
    Simple(String),
    /// Object form with input/output paths.
    Object {
        /// Path to the source index.html.
        input: String,
        /// Output filename (defaults to `"index.html"`).
        output: Option<String>,
    },
}

/// A style entry: string or `{ input, inject, bundleName }` object.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawStyleEntry {
    /// Simple string path (e.g. `"src/styles.css"`).
    Simple(String),
    /// Object form with options.
    Object {
        /// Path to the style file.
        input: String,
        /// Whether to inject into index.html (default: true).
        inject: Option<bool>,
        /// Custom bundle name.
        #[serde(rename = "bundleName")]
        bundle_name: Option<String>,
    },
}

/// An asset entry: string or `{ glob, input, output, ignore }` object.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawAssetEntry {
    /// Simple string path (e.g. `"src/assets"` or `"src/favicon.ico"`).
    Simple(String),
    /// Object form with glob pattern.
    Object {
        /// Glob pattern to match files.
        glob: String,
        /// Input base directory.
        input: String,
        /// Output directory relative to output path.
        output: Option<String>,
        /// Patterns to ignore.
        ignore: Option<Vec<String>>,
    },
}

/// A file replacement entry for environment swapping.
#[derive(Debug, Deserialize, Clone)]
pub struct FileReplacement {
    /// Path to the file to be replaced.
    pub replace: String,
    /// Path to the replacement file.
    #[serde(rename = "with")]
    pub with_file: String,
}

/// Build configuration overrides (e.g. production, development).
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawBuildConfiguration {
    /// File replacement entries.
    pub file_replacements: Option<Vec<FileReplacement>>,
}

// ---------------------------------------------------------------------------
// Resolved types (flattened, absolute paths)
// ---------------------------------------------------------------------------

/// A resolved style entry with an absolute path.
#[derive(Debug, Clone)]
pub struct ResolvedStyle {
    /// Absolute path to the style file.
    pub path: PathBuf,
    /// Whether this style should be injected into index.html.
    pub inject: bool,
    /// Custom bundle name (None means default `"styles"`).
    pub bundle_name: Option<String>,
}

/// A resolved asset entry.
#[derive(Debug, Clone)]
pub enum ResolvedAsset {
    /// A file or directory path to copy directly.
    Path(PathBuf),
    /// A glob-based asset with input directory and output mapping.
    Glob {
        /// Glob pattern to match.
        pattern: String,
        /// Absolute path to the input base directory.
        input: PathBuf,
        /// Relative output directory.
        output: String,
        /// Patterns to ignore.
        ignore: Vec<String>,
    },
}

/// A fully resolved Angular project build configuration.
#[derive(Debug, Clone)]
pub struct ResolvedAngularProject {
    /// Path to the angular.json file this was loaded from.
    pub angular_json_path: PathBuf,
    /// The project name.
    pub project_name: String,
    /// Absolute root directory of the project.
    pub root: PathBuf,
    /// Absolute source root directory.
    pub source_root: PathBuf,
    /// Absolute output path for the build.
    pub output_path: PathBuf,
    /// Absolute path to the source index.html (if configured).
    pub index_html: Option<PathBuf>,
    /// Output filename for index.html.
    pub index_output: String,
    /// Absolute path to the tsConfig file.
    pub ts_config: PathBuf,
    /// Resolved style entries.
    pub styles: Vec<ResolvedStyle>,
    /// Resolved asset entries.
    pub assets: Vec<ResolvedAsset>,
    /// Polyfill package/path entries.
    pub polyfills: Vec<String>,
    /// File replacements for the active configuration.
    pub file_replacements: Vec<FileReplacement>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse angular.json and resolve the build configuration for a project.
///
/// Reads the file at `angular_json_path`, looks up the project by name
/// (or picks the first project if `project_name` is `None`), and resolves
/// all paths relative to the angular.json directory. If `configuration` is
/// provided, merges that configuration's `fileReplacements`. If `None`,
/// uses `defaultConfiguration`.
pub fn resolve_angular_project(
    angular_json_path: &Path,
    project_name: Option<&str>,
    configuration: Option<&str>,
) -> NgcResult<ResolvedAngularProject> {
    let content = std::fs::read_to_string(angular_json_path).map_err(|e| NgcError::Io {
        path: angular_json_path.to_path_buf(),
        source: e,
    })?;

    let raw: RawAngularJson =
        serde_json::from_str(&content).map_err(|e| NgcError::AngularJsonParse {
            path: angular_json_path.to_path_buf(),
            source: e,
        })?;

    // Pick the requested project or the first one
    let (name, project) = match project_name {
        Some(name) => {
            let proj = raw
                .projects
                .get(name)
                .ok_or_else(|| NgcError::ProjectNotFound {
                    name: name.to_string(),
                    path: angular_json_path.to_path_buf(),
                })?;
            (name.to_string(), proj.clone())
        }
        None => {
            let (name, proj) =
                raw.projects
                    .into_iter()
                    .next()
                    .ok_or_else(|| NgcError::ProjectNotFound {
                        name: "<any>".to_string(),
                        path: angular_json_path.to_path_buf(),
                    })?;
            (name, proj)
        }
    };

    let base_dir = angular_json_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    let root = base_dir.join(project.root.as_deref().unwrap_or(""));
    let source_root = base_dir.join(project.source_root.as_deref().unwrap_or("src"));

    let build_target = project.architect.as_ref().and_then(|a| a.build.as_ref());

    let options = build_target.and_then(|bt| bt.options.as_ref());

    // Determine which configuration to use
    let config_name = configuration
        .map(String::from)
        .or_else(|| build_target.and_then(|bt| bt.default_configuration.clone()));

    let build_config = config_name.as_deref().and_then(|cn| {
        build_target
            .and_then(|bt| bt.configurations.as_ref())
            .and_then(|configs| configs.get(cn))
    });

    // Resolve output path
    let output_path = options
        .and_then(|o| o.output_path.as_ref())
        .map(|raw_op| resolve_output_path(raw_op, &base_dir))
        .unwrap_or_else(|| base_dir.join("dist"));

    // Resolve index
    let (index_html, index_output) = options
        .and_then(|o| o.index.as_ref())
        .map(|raw_idx| resolve_index(raw_idx, &base_dir))
        .unwrap_or((None, "index.html".to_string()));

    // Resolve tsConfig
    let ts_config = options
        .and_then(|o| o.ts_config.as_ref())
        .map(|tc| base_dir.join(tc))
        .unwrap_or_else(|| base_dir.join("tsconfig.app.json"));

    // Resolve styles
    let styles = options
        .and_then(|o| o.styles.as_ref())
        .map(|raw_styles| resolve_styles(raw_styles, &base_dir))
        .unwrap_or_default();

    // Resolve assets
    let assets = options
        .and_then(|o| o.assets.as_ref())
        .map(|raw_assets| resolve_assets(raw_assets, &base_dir))
        .unwrap_or_default();

    // Resolve polyfills
    let polyfills = options
        .and_then(|o| o.polyfills.clone())
        .unwrap_or_default();

    // Merge file replacements from active configuration
    let file_replacements = build_config
        .and_then(|bc| bc.file_replacements.clone())
        .unwrap_or_default();

    debug!(
        project = %name,
        output_path = %output_path.display(),
        config = ?config_name,
        "resolved angular.json project"
    );

    Ok(ResolvedAngularProject {
        angular_json_path: angular_json_path.to_path_buf(),
        project_name: name,
        root,
        source_root,
        output_path,
        index_html,
        index_output,
        ts_config,
        styles,
        assets,
        polyfills,
        file_replacements,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve the output path from the raw field (string or object).
fn resolve_output_path(raw: &RawOutputPath, base_dir: &Path) -> PathBuf {
    match raw {
        RawOutputPath::Simple(s) => base_dir.join(s),
        RawOutputPath::Object { base, browser, .. } => {
            let mut path = base_dir.join(base.as_deref().unwrap_or("dist"));
            if let Some(b) = browser {
                if !b.is_empty() {
                    path = path.join(b);
                }
            }
            path
        }
    }
}

/// Resolve a raw index entry to (optional input path, output filename).
fn resolve_index(raw: &RawIndex, base_dir: &Path) -> (Option<PathBuf>, String) {
    match raw {
        RawIndex::Simple(s) => (Some(base_dir.join(s)), "index.html".to_string()),
        RawIndex::Object { input, output } => (
            Some(base_dir.join(input)),
            output.clone().unwrap_or_else(|| "index.html".to_string()),
        ),
    }
}

/// Resolve raw style entries to absolute paths with metadata.
fn resolve_styles(raw: &[RawStyleEntry], base_dir: &Path) -> Vec<ResolvedStyle> {
    raw.iter()
        .map(|entry| match entry {
            RawStyleEntry::Simple(s) => ResolvedStyle {
                path: base_dir.join(s),
                inject: true,
                bundle_name: None,
            },
            RawStyleEntry::Object {
                input,
                inject,
                bundle_name,
            } => ResolvedStyle {
                path: base_dir.join(input),
                inject: inject.unwrap_or(true),
                bundle_name: bundle_name.clone(),
            },
        })
        .collect()
}

/// Resolve raw asset entries to absolute paths or glob specs.
fn resolve_assets(raw: &[RawAssetEntry], base_dir: &Path) -> Vec<ResolvedAsset> {
    raw.iter()
        .map(|entry| match entry {
            RawAssetEntry::Simple(s) => ResolvedAsset::Path(base_dir.join(s)),
            RawAssetEntry::Object {
                glob,
                input,
                output,
                ignore,
            } => ResolvedAsset::Glob {
                pattern: glob.clone(),
                input: base_dir.join(input),
                output: output.clone().unwrap_or_else(|| "/".to_string()),
                ignore: ignore.clone().unwrap_or_default(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_json(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("create temp file");
        f.write_all(content.as_bytes()).expect("write temp file");
        f
    }

    #[test]
    fn test_parse_minimal_angular_json() {
        let json = r#"{
            "projects": {
                "my-app": {
                    "root": "",
                    "sourceRoot": "src",
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist/my-app",
                                "tsConfig": "tsconfig.app.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.project_name, "my-app");
        assert!(result.output_path.ends_with("dist/my-app"));
        assert!(result.ts_config.ends_with("tsconfig.app.json"));
    }

    #[test]
    fn test_parse_object_output_path() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": { "base": "dist", "browser": "app" },
                                "tsConfig": "tsconfig.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.output_path.ends_with("dist/app"));
    }

    #[test]
    fn test_parse_object_index() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "index": { "input": "src/index.html", "output": "main.html" }
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result
            .index_html
            .as_ref()
            .unwrap()
            .ends_with("src/index.html"));
        assert_eq!(result.index_output, "main.html");
    }

    #[test]
    fn test_parse_mixed_styles() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "styles": [
                                    "src/styles.css",
                                    { "input": "src/theme.css", "inject": false, "bundleName": "theme" }
                                ]
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.styles.len(), 2);
        assert!(result.styles[0].inject);
        assert!(!result.styles[1].inject);
        assert_eq!(result.styles[1].bundle_name.as_deref(), Some("theme"));
    }

    #[test]
    fn test_parse_mixed_assets() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "assets": [
                                    "src/favicon.ico",
                                    { "glob": "**/*", "input": "src/assets", "output": "/assets/" }
                                ]
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.assets.len(), 2);
        assert!(matches!(result.assets[0], ResolvedAsset::Path(_)));
        assert!(matches!(result.assets[1], ResolvedAsset::Glob { .. }));
    }

    #[test]
    fn test_configuration_file_replacements() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json"
                            },
                            "configurations": {
                                "production": {
                                    "fileReplacements": [{
                                        "replace": "src/environments/environment.ts",
                                        "with": "src/environments/environment.prod.ts"
                                    }]
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, Some("production")).unwrap();
        assert_eq!(result.file_replacements.len(), 1);
        assert_eq!(
            result.file_replacements[0].replace,
            "src/environments/environment.ts"
        );
    }

    #[test]
    fn test_project_not_found() {
        let json = r#"{ "projects": { "app": {} } }"#;
        let f = write_temp_json(json);
        let err = resolve_angular_project(f.path(), Some("nonexistent"), None).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn test_default_configuration() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json"
                            },
                            "configurations": {
                                "production": {
                                    "fileReplacements": [{
                                        "replace": "env.ts",
                                        "with": "env.prod.ts"
                                    }]
                                }
                            },
                            "defaultConfiguration": "production"
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        // No explicit configuration — should use defaultConfiguration
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.file_replacements.len(), 1);
    }

    #[test]
    fn test_parse_polyfills() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "polyfills": ["zone.js", "zone.js/testing"]
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.polyfills, vec!["zone.js", "zone.js/testing"]);
    }
}
