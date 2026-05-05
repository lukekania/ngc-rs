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
    /// Per-project i18n configuration: source locale + map of target locales
    /// to translation files.
    pub i18n: Option<RawI18nConfig>,
}

/// Raw i18n block from `angular.json`:
/// ```jsonc
/// {
///   "sourceLocale": "en-US",
///   "locales": {
///     "de": "src/locale/messages.de.xlf",
///     "fr": { "translation": "src/locale/messages.fr.xlf", "baseHref": "/fr/" }
///   }
/// }
/// ```
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawI18nConfig {
    /// Source language code (e.g. `"en-US"`); defaults to `en-US` when absent.
    pub source_locale: Option<RawSourceLocale>,
    /// Map of target locale code → translation file (or object form).
    pub locales: Option<HashMap<String, RawLocaleEntry>>,
}

/// `sourceLocale` accepts either a bare string or an object with `code`/`baseHref`.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawSourceLocale {
    /// Simple `"en-US"` form.
    Code(String),
    /// Object form `{ "code": "en-US", "baseHref": "/" }`.
    Object {
        /// Locale code.
        code: String,
        /// Base-href applied to the source-locale build.
        #[serde(rename = "baseHref")]
        base_href: Option<String>,
    },
}

/// `locales[code]` accepts either a translation-file path or an object form
/// with `translation` and optional `baseHref`.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawLocaleEntry {
    /// Simple `"src/locale/messages.de.xlf"` form.
    Simple(String),
    /// Object form `{ "translation": "...", "baseHref": "/de/" }`.
    Object {
        /// Path to the translation file.
        translation: Option<String>,
        /// Base-href applied to this locale's build.
        #[serde(rename = "baseHref")]
        base_href: Option<String>,
    },
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
    /// Base URL prefix injected into `<base href>` in `index.html`.
    pub base_href: Option<String>,
    /// Absolute URL prefix prepended to emitted script/style URLs.
    pub deploy_url: Option<String>,
    /// `crossorigin` attribute value for injected script/link tags
    /// (`"none"`, `"anonymous"`, or `"use-credentials"`).
    pub cross_origin: Option<String>,
    /// Whether to compute and inject SRI `integrity` attributes.
    pub subresource_integrity: Option<bool>,
    /// Language for inline component styles: `css` (default), `scss`, `sass`,
    /// `less`, or `stylus`. Used when a component uses a `styles: [...]`
    /// literal rather than `styleUrl`/`styleUrls`.
    pub inline_style_language: Option<String>,
    /// Enables Angular's service-worker pipeline. Accepts either a boolean
    /// (`true`/`false`) or a string path to `ngsw-config.json` (legacy
    /// Angular `<` v15 form, where the field served double-duty as both the
    /// enable flag and the config path).
    pub service_worker: Option<RawServiceWorker>,
    /// Path to the service-worker configuration file. Defaults to
    /// `ngsw-config.json` at the workspace root when omitted.
    pub ngsw_config_path: Option<String>,
    /// Per-bundle and per-initial size budgets.
    pub budgets: Option<Vec<RawBudget>>,
    /// Build-time string-replacement map. Each value is a raw JS source
    /// fragment (e.g. `"\"https://api.example.com\""` for a string literal,
    /// `"42"` for a number, `"true"` for a boolean). Mirrors the `define`
    /// option of `@angular/build:application`, which itself mirrors
    /// esbuild's `--define`.
    pub define: Option<HashMap<String, String>>,
    /// Global script entries injected into the build (analytics snippets,
    /// CDN libraries, polyfill shims that don't fit through `polyfills.ts`).
    /// Each entry is a string path or `{ input, inject, bundleName }` object.
    pub scripts: Option<Vec<RawScriptEntry>>,
}

/// One entry in `architect.build.options.budgets` (or in a per-configuration
/// override). Mirrors the schema accepted by `@angular/build:application`.
/// Sizes accept either a raw byte count (number) or a string with a unit
/// suffix (`"500kb"`, `"1.2mb"`, `"800b"`); the parser follows ng build's
/// convention where `kb` means `kibibyte` (×1024) — confusing but compatible.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawBudget {
    /// Budget type. One of: `initial`, `anyComponentStyle`, `anyScript`,
    /// `bundle`, `all`, `allScript`. Unknown types are ignored at resolve
    /// time with a warning rather than failing the parse.
    #[serde(rename = "type")]
    pub kind: String,
    /// Required when `kind = "bundle"` — the bundle name to apply to.
    pub name: Option<String>,
    /// Baseline used for delta budgets (`"+10%"` etc.). Currently parsed
    /// but not enforced; we treat absolute sizes only.
    pub baseline: Option<RawBudgetSize>,
    /// Threshold above which a warning is emitted.
    pub maximum_warning: Option<RawBudgetSize>,
    /// Threshold above which an error is emitted (build fails).
    pub maximum_error: Option<RawBudgetSize>,
    /// Shorthand alias for `maximum_warning` when no separate min/max
    /// distinction is needed.
    pub warning: Option<RawBudgetSize>,
    /// Shorthand alias for `maximum_error`.
    pub error: Option<RawBudgetSize>,
}

/// A budget threshold value: either a raw byte count or a size string with
/// a unit suffix.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawBudgetSize {
    /// Raw byte count (`"maximumError": 1048576`).
    Number(u64),
    /// Size string with optional unit suffix (`"1mb"`, `"500kb"`, `"800b"`).
    /// Percentage strings (`"5%"`) are accepted by the parser but are not
    /// honoured by the enforcer in v1.0 (treated as 0 → never trips).
    String(String),
}

impl RawBudgetSize {
    /// Parse the value into an absolute byte count. Returns `None` for
    /// unparseable strings or percentage values (which would require a
    /// baseline to resolve).
    pub fn to_bytes(&self) -> Option<u64> {
        match self {
            RawBudgetSize::Number(n) => Some(*n),
            RawBudgetSize::String(s) => parse_size_string(s),
        }
    }
}

/// Parse an Angular size string like `"500kb"`, `"1.2mb"`, `"800b"` into a
/// byte count. Matches ng build's behaviour: `kb` and `mb` use multiples of
/// 1024 (kibibyte/mebibyte) despite the misleading name. Percentage
/// strings (`"10%"`) return `None` — delta budgets aren't supported in v1.0.
fn parse_size_string(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.ends_with('%') {
        return None;
    }
    let (num_part, multiplier) = if let Some(num) = s.strip_suffix("gb") {
        (num.trim(), 1024u64 * 1024 * 1024)
    } else if let Some(num) = s.strip_suffix("mb") {
        (num.trim(), 1024u64 * 1024)
    } else if let Some(num) = s.strip_suffix("kb") {
        (num.trim(), 1024u64)
    } else if let Some(num) = s.strip_suffix('b') {
        (num.trim(), 1u64)
    } else {
        (s.as_str(), 1u64)
    };
    let value: f64 = num_part.parse().ok()?;
    Some((value * multiplier as f64) as u64)
}

/// `serviceWorker` accepts either a bool (Angular 15+) or a string path
/// to the config file (legacy Angular `<` v15).
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawServiceWorker {
    /// Boolean form: `"serviceWorker": true`.
    Enabled(bool),
    /// Legacy string form: `"serviceWorker": "ngsw-config.json"`. The string
    /// is the path to the config file (treated as enabled when present).
    ConfigPath(String),
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

/// A global script entry: string path or `{ input, inject, bundleName }`
/// object. Mirrors the `scripts` field accepted by
/// `@angular/build:application` — entries are concatenated per `bundleName`
/// (default `"scripts"`) into a single non-module JS file injected into
/// `index.html` as `<script defer>` when `inject` is true (default).
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum RawScriptEntry {
    /// Simple string path (e.g. `"src/global.js"`).
    Simple(String),
    /// Object form with options.
    Object {
        /// Path to the script file.
        input: String,
        /// Whether to inject into index.html (default: true).
        inject: Option<bool>,
        /// Custom bundle name. Entries that share a bundle name are
        /// concatenated. Default: `"scripts"`.
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
    /// Override for `baseHref`.
    pub base_href: Option<String>,
    /// Override for `deployUrl`.
    pub deploy_url: Option<String>,
    /// Override for `crossOrigin`.
    pub cross_origin: Option<String>,
    /// Override for `subresourceIntegrity`.
    pub subresource_integrity: Option<bool>,
    /// Override for `budgets` — typically only set in the `production`
    /// configuration (development builds usually have no size limits).
    pub budgets: Option<Vec<RawBudget>>,
    /// Override for `define`. Per-configuration entries are layered on top
    /// of the base `define` map: same-key entries replace the base value,
    /// keys that appear only in the base are preserved.
    pub define: Option<HashMap<String, String>>,
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

/// A resolved global script bundle. Multiple `scripts` entries that share
/// a `bundleName` are merged into one bundle: their source files are
/// concatenated (in declaration order) into `<bundle_name>.js` and a single
/// `<script defer>` tag is emitted when [`Self::inject`] is true.
#[derive(Debug, Clone)]
pub struct ResolvedScriptBundle {
    /// Bundle name (without `.js`). Default `"scripts"` when no entry in
    /// the group declares one.
    pub name: String,
    /// Absolute paths of the source files to concatenate, in declaration
    /// order from `angular.json`.
    pub sources: Vec<PathBuf>,
    /// Whether to inject a `<script defer>` tag for this bundle into the
    /// emitted `index.html`. Defaults to true; entries that opt out via
    /// `inject: false` produce a bundle that is written to disk but not
    /// referenced from the index.
    pub inject: bool,
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

/// Value of the `crossOrigin` attribute applied to injected tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossOrigin {
    /// No `crossorigin` attribute emitted (Angular default).
    #[default]
    None,
    /// `crossorigin="anonymous"`.
    Anonymous,
    /// `crossorigin="use-credentials"`.
    UseCredentials,
}

impl CrossOrigin {
    /// The attribute value as it appears in the rendered HTML, or `None`
    /// when no attribute should be emitted.
    pub fn attribute_value(self) -> Option<&'static str> {
        match self {
            CrossOrigin::None => None,
            CrossOrigin::Anonymous => Some("anonymous"),
            CrossOrigin::UseCredentials => Some("use-credentials"),
        }
    }
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
    /// `baseHref` value to inject into `<base href>`, if any.
    pub base_href: Option<String>,
    /// `deployUrl` prefix for injected asset URLs, if any.
    pub deploy_url: Option<String>,
    /// `crossOrigin` attribute for injected script/link tags.
    pub cross_origin: CrossOrigin,
    /// Whether SRI `integrity` attributes should be injected.
    pub subresource_integrity: bool,
    /// Language for inline component `styles: [\`...\`]` literals.
    pub inline_style_language: InlineStyleLanguage,
    /// Resolved i18n configuration. `None` when the project does not declare
    /// an `i18n` block.
    pub i18n: Option<I18nConfig>,
    /// `true` when `architect.build.options.serviceWorker` is set, in which
    /// case the build should emit an `ngsw.json` manifest after writing
    /// `dist/`.
    pub service_worker: bool,
    /// Absolute path to the service-worker config file
    /// (defaults to `<base_dir>/ngsw-config.json`).
    pub ngsw_config_path: PathBuf,
    /// Resolved size budgets — empty when angular.json declares none for
    /// the active configuration. Honoured by the bundler in production
    /// builds; ignored otherwise.
    pub budgets: Vec<ResolvedBudget>,
    /// Resolved `define` map (base options merged with the active
    /// configuration's overrides). Each value is a raw JS source fragment
    /// — see [`RawBuildOptions::define`] for the encoding. Empty when no
    /// `define` block is declared.
    pub define: HashMap<String, String>,
    /// Resolved global script bundles. Each entry is one emitted JS file
    /// (`<name>.js`) holding the concatenated contents of every `scripts`
    /// entry that shares the same `bundleName`. Empty when no `scripts`
    /// are declared.
    pub scripts: Vec<ResolvedScriptBundle>,
}

/// Type of a resolved size budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    /// Combined size of the initial JS + CSS payload (`main`, `polyfills`,
    /// global stylesheets — everything that loads before any lazy chunk).
    Initial,
    /// Per-component inline-style budget. We approximate this by checking
    /// the global `styles.css` bundle since ngc-rs does not yet emit
    /// per-component style bundles separately.
    AnyComponentStyle,
    /// Per-script-bundle budget. Each emitted JS chunk is checked
    /// independently against this threshold.
    AnyScript,
    /// Budget for a specific named bundle. The `name` field selects the
    /// target file (matched by basename without extension or content hash).
    Bundle,
    /// Combined size of every emitted script + stylesheet.
    All,
    /// Combined size of every emitted script.
    AllScript,
}

impl BudgetKind {
    /// Parse the angular.json `type` string. Returns `None` for unknown
    /// values; the caller logs a tracing warning and drops the entry.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "initial" => Some(BudgetKind::Initial),
            "anyComponentStyle" => Some(BudgetKind::AnyComponentStyle),
            "anyScript" => Some(BudgetKind::AnyScript),
            "bundle" => Some(BudgetKind::Bundle),
            "all" => Some(BudgetKind::All),
            "allScript" => Some(BudgetKind::AllScript),
            _ => None,
        }
    }
}

/// A budget entry resolved into absolute byte counts. Sizes that could not
/// be parsed (e.g. percentage strings for delta budgets, which require a
/// baseline) are dropped at resolve time so the enforcer doesn't have to
/// re-validate.
#[derive(Debug, Clone)]
pub struct ResolvedBudget {
    /// What the budget applies to.
    pub kind: BudgetKind,
    /// Bundle name selector — required for `Bundle`, ignored otherwise.
    pub name: Option<String>,
    /// Threshold above which a warning is emitted.
    pub maximum_warning: Option<u64>,
    /// Threshold above which a build error is emitted.
    pub maximum_error: Option<u64>,
}

/// Resolved i18n configuration with absolute paths.
#[derive(Debug, Clone)]
pub struct I18nConfig {
    /// Source locale code (e.g. `"en-US"`).
    pub source_locale: String,
    /// `baseHref` applied to the source-locale build (overrides global).
    pub source_base_href: Option<String>,
    /// Map of locale code → resolved entry. Sorted to keep iteration
    /// deterministic across runs.
    pub locales: std::collections::BTreeMap<String, LocaleEntry>,
}

/// A single resolved target locale.
#[derive(Debug, Clone)]
pub struct LocaleEntry {
    /// Locale code (e.g. `"de"`).
    pub locale: String,
    /// Absolute path to the translation file (`.xlf`, `.json`, `.arb`).
    /// `None` when the locale is declared without translations (the source
    /// locale acts as the fallback).
    pub translation_path: Option<PathBuf>,
    /// `baseHref` applied to this locale's build.
    pub base_href: Option<String>,
}

/// Language applied to inline component `styles: [\`...\`]` literals when no
/// `styleUrl`/`styleUrls` are present. Mirrors `@angular/build:application`
/// `inlineStyleLanguage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineStyleLanguage {
    /// Plain CSS — no preprocessing.
    #[default]
    Css,
    /// SCSS (indented `scss`) via the `sass` npm package.
    Scss,
    /// Sass (original indented syntax) via the `sass` npm package.
    Sass,
    /// Less via the `less` npm package.
    Less,
    /// Stylus via the `stylus` npm package.
    Stylus,
}

impl InlineStyleLanguage {
    /// Parse from the `angular.json` string value. Unknown or missing values
    /// default to `Css`.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("scss") => InlineStyleLanguage::Scss,
            Some("sass") => InlineStyleLanguage::Sass,
            Some("less") => InlineStyleLanguage::Less,
            Some("stylus") => InlineStyleLanguage::Stylus,
            _ => InlineStyleLanguage::Css,
        }
    }
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

    // Resolve output path (default to dist/{project_name} when omitted)
    let output_path = options
        .and_then(|o| o.output_path.as_ref())
        .map(|raw_op| resolve_output_path(raw_op, &base_dir))
        .unwrap_or_else(|| base_dir.join("dist").join(&name));

    // Resolve index (default to {source_root}/index.html when omitted)
    let (index_html, index_output) = options
        .and_then(|o| o.index.as_ref())
        .map(|raw_idx| resolve_index(raw_idx, &base_dir))
        .unwrap_or_else(|| {
            let default_index = source_root.join("index.html");
            if default_index.exists() {
                (Some(default_index), "index.html".to_string())
            } else {
                (None, "index.html".to_string())
            }
        });

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

    // Merge baseHref / deployUrl / crossOrigin / subresourceIntegrity with
    // configuration values taking precedence over base options.
    let base_href = build_config
        .and_then(|bc| bc.base_href.clone())
        .or_else(|| options.and_then(|o| o.base_href.clone()));
    let deploy_url = build_config
        .and_then(|bc| bc.deploy_url.clone())
        .or_else(|| options.and_then(|o| o.deploy_url.clone()));
    let cross_origin_raw = build_config
        .and_then(|bc| bc.cross_origin.clone())
        .or_else(|| options.and_then(|o| o.cross_origin.clone()));
    let cross_origin = match cross_origin_raw.as_deref() {
        Some("anonymous") => CrossOrigin::Anonymous,
        Some("use-credentials") => CrossOrigin::UseCredentials,
        _ => CrossOrigin::None,
    };
    let subresource_integrity = build_config
        .and_then(|bc| bc.subresource_integrity)
        .or_else(|| options.and_then(|o| o.subresource_integrity))
        .unwrap_or(false);
    let inline_style_language =
        InlineStyleLanguage::parse(options.and_then(|o| o.inline_style_language.as_deref()));
    // serviceWorker accepts bool (modern) or string-path (legacy). When the
    // string form is used, it doubles as the ngsw config path unless an
    // explicit `ngswConfigPath` overrides it.
    let (service_worker, sw_inline_path) = match options.and_then(|o| o.service_worker.as_ref()) {
        Some(RawServiceWorker::Enabled(b)) => (*b, None),
        Some(RawServiceWorker::ConfigPath(p)) => (true, Some(p.clone())),
        None => (false, None),
    };
    let ngsw_config_path = options
        .and_then(|o| o.ngsw_config_path.as_deref())
        .map(String::from)
        .or(sw_inline_path)
        .map(|p| base_dir.join(p))
        .unwrap_or_else(|| base_dir.join("ngsw-config.json"));
    let i18n = project
        .i18n
        .as_ref()
        .map(|raw| resolve_i18n(raw, &base_dir));

    // Resolve budgets — per-configuration overrides take precedence over
    // base options (matches Angular CLI's merge order). When neither
    // declares budgets the resolved list is empty.
    let raw_budgets = build_config
        .and_then(|bc| bc.budgets.as_ref())
        .or_else(|| options.and_then(|o| o.budgets.as_ref()));
    let budgets = raw_budgets
        .map(|list| resolve_budgets(list))
        .unwrap_or_default();

    // Merge `define`: start from base options, then layer the active
    // configuration's overrides on top (same-key entries replace).
    let mut define: HashMap<String, String> =
        options.and_then(|o| o.define.clone()).unwrap_or_default();
    if let Some(overrides) = build_config.and_then(|bc| bc.define.as_ref()) {
        for (k, v) in overrides {
            define.insert(k.clone(), v.clone());
        }
    }

    let scripts = options
        .and_then(|o| o.scripts.as_ref())
        .map(|raw_scripts| resolve_scripts(raw_scripts, &base_dir))
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
        base_href,
        deploy_url,
        cross_origin,
        subresource_integrity,
        inline_style_language,
        i18n,
        service_worker,
        ngsw_config_path,
        budgets,
        define,
        scripts,
    })
}

/// Convert a slice of `RawBudget` entries into `ResolvedBudget`s, dropping
/// (with a tracing warning) any entries with an unknown `type` or with no
/// parseable threshold.
fn resolve_budgets(raw: &[RawBudget]) -> Vec<ResolvedBudget> {
    raw.iter()
        .filter_map(|b| {
            let kind = match BudgetKind::parse(&b.kind) {
                Some(k) => k,
                None => {
                    tracing::warn!("ignoring unknown budget type {:?} in angular.json", b.kind);
                    return None;
                }
            };
            let maximum_warning = b
                .maximum_warning
                .as_ref()
                .or(b.warning.as_ref())
                .and_then(|s| s.to_bytes());
            let maximum_error = b
                .maximum_error
                .as_ref()
                .or(b.error.as_ref())
                .and_then(|s| s.to_bytes());
            if maximum_warning.is_none() && maximum_error.is_none() {
                tracing::warn!(
                    "ignoring budget for {:?} — no maximumWarning or maximumError",
                    b.kind
                );
                return None;
            }
            Some(ResolvedBudget {
                kind,
                name: b.name.clone(),
                maximum_warning,
                maximum_error,
            })
        })
        .collect()
}

/// Resolve a `RawI18nConfig` from `angular.json` into absolute paths.
fn resolve_i18n(raw: &RawI18nConfig, base_dir: &Path) -> I18nConfig {
    let (source_locale, source_base_href) = match &raw.source_locale {
        Some(RawSourceLocale::Code(c)) => (c.clone(), None),
        Some(RawSourceLocale::Object { code, base_href }) => (code.clone(), base_href.clone()),
        None => ("en-US".to_string(), None),
    };
    let mut locales = std::collections::BTreeMap::new();
    if let Some(map) = &raw.locales {
        for (code, entry) in map {
            let resolved = match entry {
                RawLocaleEntry::Simple(path) => LocaleEntry {
                    locale: code.clone(),
                    translation_path: Some(base_dir.join(path)),
                    base_href: None,
                },
                RawLocaleEntry::Object {
                    translation,
                    base_href,
                } => LocaleEntry {
                    locale: code.clone(),
                    translation_path: translation.as_ref().map(|t| base_dir.join(t)),
                    base_href: base_href.clone(),
                },
            };
            locales.insert(code.clone(), resolved);
        }
    }
    I18nConfig {
        source_locale,
        source_base_href,
        locales,
    }
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

/// Resolve raw script entries into per-bundle groups with absolute paths.
///
/// Entries that share a `bundleName` (default `"scripts"`) are collapsed
/// into one [`ResolvedScriptBundle`], preserving declaration order so the
/// concatenation step writes them in the order the user declared. Bundle
/// order in the returned `Vec` matches the order in which each bundle
/// name first appeared.
///
/// When entries within the same bundle disagree on `inject`, the first
/// entry wins and a warning is logged — `@angular/build:application`
/// treats this as undefined behavior, so we pick a deterministic policy
/// rather than silently merging.
fn resolve_scripts(raw: &[RawScriptEntry], base_dir: &Path) -> Vec<ResolvedScriptBundle> {
    let mut bundles: Vec<ResolvedScriptBundle> = Vec::new();
    for entry in raw {
        let (input, inject, bundle_name) = match entry {
            RawScriptEntry::Simple(s) => (s.clone(), true, None),
            RawScriptEntry::Object {
                input,
                inject,
                bundle_name,
            } => (input.clone(), inject.unwrap_or(true), bundle_name.clone()),
        };
        let name = bundle_name.unwrap_or_else(|| "scripts".to_string());
        let path = base_dir.join(&input);
        if let Some(existing) = bundles.iter_mut().find(|b| b.name == name) {
            if existing.inject != inject {
                tracing::warn!(
                    bundle = %name,
                    "scripts entries in bundle {:?} disagree on `inject`; using first entry's value ({})",
                    name,
                    existing.inject
                );
            }
            existing.sources.push(path);
        } else {
            bundles.push(ResolvedScriptBundle {
                name,
                sources: vec![path],
                inject,
            });
        }
    }
    bundles
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
    fn test_parse_scripts_string_form_defaults_inject_and_bundle_name() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "scripts": ["src/global.js"]
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.scripts.len(), 1);
        let bundle = &result.scripts[0];
        assert_eq!(bundle.name, "scripts");
        assert!(bundle.inject);
        assert_eq!(bundle.sources.len(), 1);
        assert!(bundle.sources[0].ends_with("src/global.js"));
    }

    #[test]
    fn test_parse_scripts_object_form_with_bundle_name_and_inject_false() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "scripts": [
                                    { "input": "src/lazy.js", "inject": false, "bundleName": "lazy" }
                                ]
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.scripts.len(), 1);
        let bundle = &result.scripts[0];
        assert_eq!(bundle.name, "lazy");
        assert!(!bundle.inject);
        assert!(bundle.sources[0].ends_with("src/lazy.js"));
    }

    #[test]
    fn test_parse_scripts_groups_shared_bundle_name() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "scripts": [
                                    "src/a.js",
                                    { "input": "src/b.js", "bundleName": "scripts" },
                                    { "input": "node_modules/web-vitals/dist/web-vitals.iife.js", "bundleName": "vitals" }
                                ]
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.scripts.len(), 2);
        let scripts_bundle = result
            .scripts
            .iter()
            .find(|b| b.name == "scripts")
            .expect("scripts bundle present");
        assert_eq!(scripts_bundle.sources.len(), 2);
        assert!(scripts_bundle.sources[0].ends_with("src/a.js"));
        assert!(scripts_bundle.sources[1].ends_with("src/b.js"));
        let vitals = result
            .scripts
            .iter()
            .find(|b| b.name == "vitals")
            .expect("vitals bundle present");
        assert_eq!(vitals.sources.len(), 1);
    }

    #[test]
    fn test_parse_scripts_omitted_field_yields_empty_vec() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.scripts.is_empty());
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
    fn test_parse_deploy_options() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "baseHref": "/app/",
                                "deployUrl": "https://cdn.example.com/",
                                "crossOrigin": "anonymous",
                                "subresourceIntegrity": true
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.base_href.as_deref(), Some("/app/"));
        assert_eq!(
            result.deploy_url.as_deref(),
            Some("https://cdn.example.com/")
        );
        assert_eq!(result.cross_origin, CrossOrigin::Anonymous);
        assert!(result.subresource_integrity);
    }

    #[test]
    fn test_cross_origin_use_credentials() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "crossOrigin": "use-credentials"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.cross_origin, CrossOrigin::UseCredentials);
    }

    #[test]
    fn test_deploy_options_default_to_none() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.base_href.is_none());
        assert!(result.deploy_url.is_none());
        assert_eq!(result.cross_origin, CrossOrigin::None);
        assert!(!result.subresource_integrity);
    }

    #[test]
    fn test_configuration_overrides_deploy_options() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "baseHref": "/default/",
                                "subresourceIntegrity": false
                            },
                            "configurations": {
                                "production": {
                                    "baseHref": "/prod/",
                                    "subresourceIntegrity": true
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, Some("production")).unwrap();
        assert_eq!(result.base_href.as_deref(), Some("/prod/"));
        assert!(result.subresource_integrity);
    }

    #[test]
    fn test_parse_inline_style_language() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "inlineStyleLanguage": "scss"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.inline_style_language, InlineStyleLanguage::Scss);
    }

    #[test]
    fn test_inline_style_language_defaults_to_css() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": { "outputPath": "dist", "tsConfig": "tsconfig.json" }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(result.inline_style_language, InlineStyleLanguage::Css);
    }

    #[test]
    fn test_inline_style_language_less_and_stylus() {
        for (raw, expected) in [
            ("less", InlineStyleLanguage::Less),
            ("stylus", InlineStyleLanguage::Stylus),
            ("sass", InlineStyleLanguage::Sass),
        ] {
            let json = format!(
                r#"{{
                    "projects": {{
                        "app": {{
                            "architect": {{
                                "build": {{
                                    "options": {{
                                        "outputPath": "dist",
                                        "tsConfig": "tsconfig.json",
                                        "inlineStyleLanguage": "{raw}"
                                    }}
                                }}
                            }}
                        }}
                    }}
                }}"#
            );
            let f = write_temp_json(&json);
            let result = resolve_angular_project(f.path(), None, None).unwrap();
            assert_eq!(result.inline_style_language, expected);
        }
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

    #[test]
    fn test_parse_i18n_block_simple_form() {
        let json = r#"{
            "projects": {
                "app": {
                    "i18n": {
                        "sourceLocale": "en-US",
                        "locales": {
                            "de": "src/locale/messages.de.xlf",
                            "fr": { "translation": "src/locale/messages.fr.xlf", "baseHref": "/fr/" }
                        }
                    },
                    "architect": { "build": { "options": {
                        "outputPath": "dist", "tsConfig": "tsconfig.json"
                    } } }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        let i18n = result.i18n.expect("i18n block should be parsed");
        assert_eq!(i18n.source_locale, "en-US");
        let de = i18n.locales.get("de").expect("de locale present");
        assert!(de
            .translation_path
            .as_ref()
            .unwrap()
            .ends_with("src/locale/messages.de.xlf"));
        assert!(de.base_href.is_none());
        let fr = i18n.locales.get("fr").expect("fr locale present");
        assert_eq!(fr.base_href.as_deref(), Some("/fr/"));
    }

    #[test]
    fn test_parse_i18n_block_source_locale_object_form() {
        let json = r#"{
            "projects": {
                "app": {
                    "i18n": {
                        "sourceLocale": { "code": "en-US", "baseHref": "/en/" },
                        "locales": {}
                    },
                    "architect": { "build": { "options": {
                        "outputPath": "dist", "tsConfig": "tsconfig.json"
                    } } }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        let i18n = result.i18n.expect("i18n block should be parsed");
        assert_eq!(i18n.source_locale, "en-US");
        assert_eq!(i18n.source_base_href.as_deref(), Some("/en/"));
        assert!(i18n.locales.is_empty());
    }

    #[test]
    fn test_parse_service_worker_enabled() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "serviceWorker": true,
                                "ngswConfigPath": "ngsw-config.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.service_worker);
        assert!(result.ngsw_config_path.ends_with("ngsw-config.json"));
    }

    #[test]
    fn test_parse_service_worker_legacy_string_path() {
        // Pre-v15 Angular used `"serviceWorker": "ngsw-config.json"` where
        // the string both enabled the SW and named the config file.
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "serviceWorker": "ngsw-config.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.service_worker);
        assert!(result.ngsw_config_path.ends_with("ngsw-config.json"));
    }

    #[test]
    fn test_explicit_ngsw_config_path_overrides_legacy_string() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "serviceWorker": "legacy.json",
                                "ngswConfigPath": "explicit/ngsw.json"
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.service_worker);
        assert!(result.ngsw_config_path.ends_with("explicit/ngsw.json"));
    }

    #[test]
    fn test_service_worker_defaults_off() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": { "outputPath": "dist", "tsConfig": "tsconfig.json" }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(!result.service_worker);
        // Path defaults to <base_dir>/ngsw-config.json even when sw is off.
        assert!(result.ngsw_config_path.ends_with("ngsw-config.json"));
    }

    #[test]
    fn test_parse_define_base_options() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "define": {
                                    "__APP_API_URL__": "\"https://api.example.com\"",
                                    "__BUILD_VERSION__": "\"1.0.0\""
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert_eq!(
            result.define.get("__APP_API_URL__").map(String::as_str),
            Some("\"https://api.example.com\"")
        );
        assert_eq!(
            result.define.get("__BUILD_VERSION__").map(String::as_str),
            Some("\"1.0.0\"")
        );
    }

    #[test]
    fn test_define_configuration_overrides_base() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": {
                                "outputPath": "dist",
                                "tsConfig": "tsconfig.json",
                                "define": {
                                    "__API__": "\"dev\"",
                                    "__SHARED__": "true"
                                }
                            },
                            "configurations": {
                                "production": {
                                    "define": {
                                        "__API__": "\"prod\""
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, Some("production")).unwrap();
        // Configuration overrides win for collisions.
        assert_eq!(
            result.define.get("__API__").map(String::as_str),
            Some("\"prod\"")
        );
        // Base-only entries are preserved.
        assert_eq!(
            result.define.get("__SHARED__").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn test_define_defaults_to_empty_when_absent() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": {
                        "build": {
                            "options": { "outputPath": "dist", "tsConfig": "tsconfig.json" }
                        }
                    }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.define.is_empty());
    }

    #[test]
    fn test_no_i18n_block_resolves_to_none() {
        let json = r#"{
            "projects": {
                "app": {
                    "architect": { "build": { "options": {
                        "outputPath": "dist", "tsConfig": "tsconfig.json"
                    } } }
                }
            }
        }"#;
        let f = write_temp_json(json);
        let result = resolve_angular_project(f.path(), None, None).unwrap();
        assert!(result.i18n.is_none());
    }
}
