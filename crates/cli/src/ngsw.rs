//! Angular service-worker manifest (`ngsw.json`) generation.
//!
//! Reads `ngsw-config.json`, walks the populated `dist/` tree, hashes every
//! file matched by an `assetGroups` pattern (SHA-1 hex per Angular's spec),
//! and emits `dist/ngsw.json` plus copies of the worker scripts from
//! `node_modules/@angular/service-worker/`.
//!
//! Pattern syntax mirrors Angular's `@angular/service-worker/config`:
//! a leading `/` anchors at the dist root, `**` matches any number of path
//! segments, `*` matches one segment, `?` matches a single non-`/` char,
//! `(a|b)` denotes alternation, and a leading `!` negates a pattern.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ngc_diagnostics::{NgcError, NgcResult};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

// ---------------------------------------------------------------------------
// Raw deserialization types (match ngsw-config.json shape)
// ---------------------------------------------------------------------------

/// Top-level structure of `ngsw-config.json`. Unknown fields (e.g. `$schema`)
/// are silently ignored.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NgswConfig {
    /// Path to the entry HTML document, used by SwUpdate as the navigation
    /// fallback. Defaults to `/index.html` when absent.
    #[serde(default)]
    pub index: Option<String>,
    /// Asset cache groups (typically the app shell + lazy assets).
    #[serde(default)]
    pub asset_groups: Vec<AssetGroup>,
    /// Data cache groups (API responses, etc.).
    #[serde(default)]
    pub data_groups: Vec<DataGroup>,
    /// Patterns describing which navigation URLs the SW should serve
    /// `index.html` for. Defaults to `["/**"]` minus any URL with a file
    /// extension or that contains `__`.
    #[serde(default)]
    pub navigation_urls: Option<Vec<String>>,
    /// `"performance"` (default) or `"freshness"`.
    #[serde(default)]
    pub navigation_request_strategy: Option<String>,
}

/// One `assetGroups[]` entry in `ngsw-config.json`.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AssetGroup {
    /// Logical group name (e.g. `"app"`).
    pub name: String,
    /// `"prefetch"` or `"lazy"`. Defaults to `"prefetch"`.
    #[serde(default)]
    pub install_mode: Option<String>,
    /// `"prefetch"` or `"lazy"`. Defaults to the value of `installMode`.
    #[serde(default)]
    pub update_mode: Option<String>,
    /// Resource matchers — `files` (local globs) and `urls` (remote URLs).
    #[serde(default)]
    pub resources: Resources,
    /// `cacheQueryOptions.ignoreVary` flag (defaults to `true` upstream).
    #[serde(default)]
    pub cache_query_options: Option<CacheQueryOptions>,
}

/// Resource matchers inside an asset/data group.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Resources {
    /// Glob-like patterns for files included in the group.
    #[serde(default)]
    pub files: Vec<String>,
    /// Remote URL patterns. Stored verbatim — never hashed.
    #[serde(default)]
    pub urls: Vec<String>,
}

/// One `dataGroups[]` entry in `ngsw-config.json`.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DataGroup {
    /// Logical group name.
    pub name: String,
    /// URL patterns this group caches.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Numeric version used to invalidate the data cache.
    #[serde(default)]
    pub version: Option<u32>,
    /// Cache-config block (size, age, strategy).
    #[serde(default)]
    pub cache_config: CacheConfig,
}

/// `dataGroups[].cacheConfig` block.
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheConfig {
    /// Maximum number of entries kept in the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
    /// Maximum age of cache entries (Angular duration string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<String>,
    /// Network-timeout before falling back to cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// `"performance"` or `"freshness"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
}

/// `cacheQueryOptions` block.
#[derive(Debug, Deserialize, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheQueryOptions {
    /// Whether to ignore the `Vary` header when matching cached responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_vary: Option<bool>,
}

// ---------------------------------------------------------------------------
// Output (ngsw.json) types
// ---------------------------------------------------------------------------

/// Compiled `ngsw.json` manifest written into `dist/`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NgswManifest {
    /// Always `1` for the current Angular SW protocol.
    pub config_version: u32,
    /// Build-time epoch milliseconds. Drives version comparison in the SW.
    pub timestamp: u64,
    /// Navigation fallback URL.
    pub index: String,
    /// Compiled asset groups with concrete `urls` lists.
    pub asset_groups: Vec<CompiledAssetGroup>,
    /// Data groups (regex-compiled patterns).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub data_groups: Vec<CompiledDataGroup>,
    /// Regex objects describing which paths are SPA navigation requests.
    pub navigation_urls: Vec<NavigationUrl>,
    /// `"performance"` or `"freshness"`.
    pub navigation_request_strategy: String,
    /// Map of asset URL → SHA-1 hex digest.
    pub hash_table: BTreeMap<String, String>,
}

/// A fully resolved asset group ready for the SW runtime.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompiledAssetGroup {
    /// Group name carried over from the config.
    pub name: String,
    /// `"prefetch"` or `"lazy"`.
    pub install_mode: String,
    /// `"prefetch"` or `"lazy"`.
    pub update_mode: String,
    /// `cacheQueryOptions` with sensible defaults.
    pub cache_query_options: CacheQueryOptions,
    /// Concrete URLs (root-relative) that match the file patterns.
    pub urls: Vec<String>,
    /// Regex strings derived from the original `urls`/external patterns.
    pub patterns: Vec<String>,
}

/// A fully resolved data group ready for the SW runtime.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompiledDataGroup {
    /// Group name carried over from the config.
    pub name: String,
    /// Numeric version (defaults to `1`).
    pub version: u32,
    /// `cacheQueryOptions` with sensible defaults.
    pub cache_query_options: CacheQueryOptions,
    /// Regex strings derived from the original `urls`.
    pub patterns: Vec<String>,
    /// Cache-config (max age, size, strategy).
    pub cache_config: SerializedCacheConfig,
}

/// Cache-config with defaults filled in for serialization.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedCacheConfig {
    /// Maximum number of entries.
    pub max_size: u64,
    /// Maximum entry age.
    pub max_age: String,
    /// Network timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// `"performance"` or `"freshness"`.
    pub strategy: String,
}

/// One entry in `navigationUrls`.
#[derive(Debug, Serialize)]
pub struct NavigationUrl {
    /// `true` for include patterns, `false` for excludes.
    pub positive: bool,
    /// Regex source string.
    pub regex: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load `ngsw-config.json` from disk.
pub fn load_config(path: &Path) -> NgcResult<NgswConfig> {
    let content = std::fs::read_to_string(path).map_err(|e| NgcError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    serde_json::from_str(&content).map_err(|e| NgcError::ConfigError {
        message: format!(
            "failed to parse ngsw-config.json at {}: {e}",
            path.display()
        ),
    })
}

/// Generate `dist/ngsw.json` from the populated `dist_dir` and the loaded
/// service-worker config. Returns the path that was written.
///
/// `timestamp` is injected so callers (notably tests) can produce stable
/// output. Production callers pass the wall-clock epoch ms.
pub fn generate_manifest(
    dist_dir: &Path,
    config: &NgswConfig,
    timestamp: u64,
) -> NgcResult<PathBuf> {
    // Collect every file under dist/, indexed by root-relative URL.
    let entries = collect_dist_entries(dist_dir)?;

    let mut hash_table: BTreeMap<String, String> = BTreeMap::new();
    let mut already_assigned: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Build asset groups: walk patterns, materialize matching URLs, hash each.
    let mut compiled_asset_groups = Vec::with_capacity(config.asset_groups.len());
    for group in &config.asset_groups {
        let install_mode = group
            .install_mode
            .clone()
            .unwrap_or_else(|| "prefetch".into());
        let update_mode = group
            .update_mode
            .clone()
            .unwrap_or_else(|| install_mode.clone());

        let (positive, negative) = compile_file_patterns(&group.resources.files)?;
        let mut urls: Vec<String> = Vec::new();
        for entry in &entries {
            if !pattern_matches(&entry.url, &positive, &negative) {
                continue;
            }
            // First group to claim a URL owns it (Angular's behavior — once an
            // asset is in a prefetch group, later groups don't re-list it).
            if already_assigned.contains(&entry.url) {
                continue;
            }
            already_assigned.insert(entry.url.clone());
            urls.push(entry.url.clone());
            hash_table.insert(entry.url.clone(), entry.hash.clone());
        }
        urls.sort();

        let patterns: Vec<String> = group
            .resources
            .urls
            .iter()
            .map(|u| url_pattern_to_regex(u))
            .collect();

        compiled_asset_groups.push(CompiledAssetGroup {
            name: group.name.clone(),
            install_mode,
            update_mode,
            cache_query_options: resolve_cache_query_options(group.cache_query_options.as_ref()),
            urls,
            patterns,
        });
    }

    let mut compiled_data_groups = Vec::with_capacity(config.data_groups.len());
    for group in &config.data_groups {
        let patterns: Vec<String> = group.urls.iter().map(|u| url_pattern_to_regex(u)).collect();
        compiled_data_groups.push(CompiledDataGroup {
            name: group.name.clone(),
            version: group.version.unwrap_or(1),
            cache_query_options: CacheQueryOptions {
                ignore_vary: Some(true),
            },
            patterns,
            cache_config: SerializedCacheConfig {
                max_size: group.cache_config.max_size.unwrap_or(0),
                max_age: group
                    .cache_config
                    .max_age
                    .clone()
                    .unwrap_or_else(|| "0u".to_string()),
                timeout: group.cache_config.timeout.clone(),
                strategy: group
                    .cache_config
                    .strategy
                    .clone()
                    .unwrap_or_else(|| "performance".to_string()),
            },
        });
    }

    let navigation_urls = build_navigation_urls(config.navigation_urls.as_deref());
    let navigation_request_strategy = config
        .navigation_request_strategy
        .clone()
        .unwrap_or_else(|| "performance".to_string());

    let manifest = NgswManifest {
        config_version: 1,
        timestamp,
        index: config.index.clone().unwrap_or_else(|| "/index.html".into()),
        asset_groups: compiled_asset_groups,
        data_groups: compiled_data_groups,
        navigation_urls,
        navigation_request_strategy,
        hash_table,
    };

    let out_path = dist_dir.join("ngsw.json");
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| NgcError::JsonOutputError {
        message: format!("ngsw.json: {e}"),
    })?;
    std::fs::write(&out_path, json).map_err(|e| NgcError::Io {
        path: out_path.clone(),
        source: e,
    })?;
    Ok(out_path)
}

/// Copy `ngsw-worker.js` (and optionally `safety-worker.js`) from
/// `node_modules/@angular/service-worker/` into `dist_dir`. Returns the
/// list of files written. Missing source files are silently skipped — apps
/// without `@angular/service-worker` installed simply get an `ngsw.json`
/// without the worker script (the issue's DoD only requires the manifest).
pub fn copy_worker_scripts(dist_dir: &Path, project_root: &Path) -> NgcResult<Vec<PathBuf>> {
    let mut copied = Vec::new();
    let sw_dir = project_root
        .join("node_modules")
        .join("@angular")
        .join("service-worker");
    for name in ["ngsw-worker.js", "safety-worker.js"] {
        let src = sw_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let dst = dist_dir.join(name);
        std::fs::copy(&src, &dst).map_err(|e| NgcError::Io {
            path: dst.clone(),
            source: e,
        })?;
        copied.push(dst);
    }
    Ok(copied)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct DistEntry {
    url: String,
    hash: String,
}

/// Walk `dist_dir` recursively, hashing every file. Skips the manifest itself
/// and the worker scripts, which must not be cached as content.
fn collect_dist_entries(dist_dir: &Path) -> NgcResult<Vec<DistEntry>> {
    let mut out = Vec::new();
    walk_dir(dist_dir, dist_dir, &mut out)?;
    out.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(out)
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<DistEntry>) -> NgcResult<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| NgcError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| NgcError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| NgcError::Io {
            path: path.clone(),
            source: e,
        })?;
        if file_type.is_dir() {
            walk_dir(root, &path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // Don't hash files we're about to write or that the SW must not
        // cache (the worker scripts themselves and any prior manifest).
        if matches!(name, "ngsw.json" | "ngsw-worker.js" | "safety-worker.js") {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let mut url = String::from("/");
        for (i, comp) in rel.components().enumerate() {
            if i > 0 {
                url.push('/');
            }
            url.push_str(&comp.as_os_str().to_string_lossy());
        }
        let bytes = std::fs::read(&path).map_err(|e| NgcError::Io {
            path: path.clone(),
            source: e,
        })?;
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        let hash = hex_lower(&hasher.finalize());
        out.push(DistEntry { url, hash });
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Compile `files` patterns into positive and negative regex sets.
/// Patterns prefixed with `!` are treated as negations.
fn compile_file_patterns(patterns: &[String]) -> NgcResult<(Vec<Regex>, Vec<Regex>)> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for pat in patterns {
        let (negate, body) = match pat.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pat.as_str()),
        };
        let re_src = glob_to_regex(body);
        let re = Regex::new(&re_src).map_err(|e| NgcError::ConfigError {
            message: format!("ngsw-config.json: invalid pattern {pat:?}: {e}"),
        })?;
        if negate {
            negative.push(re);
        } else {
            positive.push(re);
        }
    }
    Ok((positive, negative))
}

fn pattern_matches(url: &str, positive: &[Regex], negative: &[Regex]) -> bool {
    if !positive.iter().any(|r| r.is_match(url)) {
        return false;
    }
    if negative.iter().any(|r| r.is_match(url)) {
        return false;
    }
    true
}

/// Translate Angular's glob syntax to a regex anchored over a full URL.
///
/// This handles `**`, `*`, `?`, `(a|b)` alt groups, `[chars]` classes, and
/// escapes regex metacharacters. Patterns without a leading `/` are treated
/// as anchored to dist root with an implicit `**/` prefix (matches Angular's
/// "match anywhere" semantics).
fn glob_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let leading_slash = pattern.starts_with('/');
    if !leading_slash {
        // Implicit "match anywhere": prepend (?:.*/)? so the pattern can match
        // a path segment at any depth. Angular's compiler does the same for
        // un-anchored patterns.
        out.push_str("(?:/.*)?/");
    }
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push_str(".*");
                    i += 2;
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '(' | ')' | '|' => {
                out.push(c);
                i += 1;
            }
            '[' => {
                // Pass through [..] character class, escaping nothing.
                let mut j = i + 1;
                out.push('[');
                while j < chars.len() && chars[j] != ']' {
                    out.push(chars[j]);
                    j += 1;
                }
                if j < chars.len() {
                    out.push(']');
                    i = j + 1;
                } else {
                    // Unterminated class: leave verbatim.
                    i = j;
                }
            }
            '.' | '+' | '$' | '^' | '{' | '}' | '\\' => {
                out.push('\\');
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.push('$');
    out
}

/// Translate a remote URL pattern (used in `urls` arrays of asset/data
/// groups) into a regex. Same rules as `glob_to_regex`, but without the
/// leading-slash anchoring tweak — these patterns are matched against full
/// URLs by the SW runtime.
fn url_pattern_to_regex(pattern: &str) -> String {
    glob_to_regex(pattern)
}

fn resolve_cache_query_options(opts: Option<&CacheQueryOptions>) -> CacheQueryOptions {
    let ignore_vary = opts.and_then(|o| o.ignore_vary).unwrap_or(true);
    CacheQueryOptions {
        ignore_vary: Some(ignore_vary),
    }
}

/// Build the `navigationUrls` list — applying Angular's defaults when none
/// are configured. The defaults are: include everything, but exclude any URL
/// that contains a file extension or `__`.
fn build_navigation_urls(configured: Option<&[String]>) -> Vec<NavigationUrl> {
    let patterns: Vec<String> = match configured {
        Some(p) if !p.is_empty() => p.iter().map(String::from).collect(),
        _ => vec![
            "/**".to_string(),
            "!/**/*.*".to_string(),
            "!/**/*__*".to_string(),
            "!/**/*__*/**".to_string(),
        ],
    };
    patterns
        .into_iter()
        .map(|raw| {
            let (positive, body) = match raw.strip_prefix('!') {
                Some(rest) => (false, rest.to_string()),
                None => (true, raw),
            };
            NavigationUrl {
                positive,
                regex: glob_to_regex(&body),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn glob_root_double_star_matches_nested() {
        let re = Regex::new(&glob_to_regex("/**")).unwrap();
        assert!(re.is_match("/index.html"));
        assert!(re.is_match("/assets/icon.png"));
        assert!(re.is_match("/a/b/c.txt"));
    }

    #[test]
    fn glob_single_star_does_not_cross_slash() {
        let re = Regex::new(&glob_to_regex("/*.js")).unwrap();
        assert!(re.is_match("/main.js"));
        assert!(!re.is_match("/sub/main.js"));
    }

    #[test]
    fn glob_alt_group() {
        let re = Regex::new(&glob_to_regex("/*.(svg|png|jpg)")).unwrap();
        assert!(re.is_match("/icon.svg"));
        assert!(re.is_match("/photo.jpg"));
        assert!(!re.is_match("/main.js"));
    }

    #[test]
    fn negative_pattern_excludes() {
        let (pos, neg) =
            compile_file_patterns(&["/**".to_string(), "!/3rdpartylicenses.txt".to_string()])
                .unwrap();
        assert!(pattern_matches("/index.html", &pos, &neg));
        assert!(!pattern_matches("/3rdpartylicenses.txt", &pos, &neg));
    }

    #[test]
    fn generate_manifest_hashes_assets() {
        let dir = tempfile::tempdir().unwrap();
        let dist = dir.path();
        write(&dist.join("index.html"), "<!doctype html><title>x</title>");
        write(&dist.join("main-ABCD.js"), "console.log('hi')");
        write(&dist.join("assets/logo.svg"), "<svg/>");

        let config = NgswConfig {
            index: Some("/index.html".into()),
            asset_groups: vec![
                AssetGroup {
                    name: "app".into(),
                    install_mode: Some("prefetch".into()),
                    update_mode: None,
                    resources: Resources {
                        files: vec!["/index.html".into(), "/*.js".into(), "/*.css".into()],
                        urls: vec![],
                    },
                    cache_query_options: None,
                },
                AssetGroup {
                    name: "assets".into(),
                    install_mode: Some("lazy".into()),
                    update_mode: Some("prefetch".into()),
                    resources: Resources {
                        files: vec!["/assets/**".into()],
                        urls: vec![],
                    },
                    cache_query_options: None,
                },
            ],
            data_groups: vec![],
            navigation_urls: None,
            navigation_request_strategy: None,
        };

        let path = generate_manifest(dist, &config, 1_700_000_000_000).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["configVersion"], 1);
        assert_eq!(parsed["index"], "/index.html");
        assert_eq!(parsed["timestamp"], 1_700_000_000_000_u64);

        // Asset groups carry resolved URL lists.
        let groups = parsed["assetGroups"].as_array().unwrap();
        let app_urls = groups[0]["urls"].as_array().unwrap();
        let app_url_strs: Vec<&str> = app_urls.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(app_url_strs.contains(&"/index.html"));
        assert!(app_url_strs.contains(&"/main-ABCD.js"));
        let asset_urls = groups[1]["urls"].as_array().unwrap();
        assert_eq!(asset_urls[0], "/assets/logo.svg");

        // hashTable contains every URL with a 40-char SHA-1 hex value.
        let table = parsed["hashTable"].as_object().unwrap();
        assert_eq!(table.len(), 3);
        for v in table.values() {
            assert_eq!(v.as_str().unwrap().len(), 40);
            assert!(v.as_str().unwrap().chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn url_only_owned_by_first_matching_group() {
        // /index.html matches both groups' patterns; the first should claim it.
        let dir = tempfile::tempdir().unwrap();
        let dist = dir.path();
        write(&dist.join("index.html"), "x");

        let config = NgswConfig {
            index: Some("/index.html".into()),
            asset_groups: vec![
                AssetGroup {
                    name: "app".into(),
                    install_mode: Some("prefetch".into()),
                    update_mode: None,
                    resources: Resources {
                        files: vec!["/index.html".into()],
                        urls: vec![],
                    },
                    cache_query_options: None,
                },
                AssetGroup {
                    name: "fallback".into(),
                    install_mode: Some("lazy".into()),
                    update_mode: None,
                    resources: Resources {
                        files: vec!["/**".into()],
                        urls: vec![],
                    },
                    cache_query_options: None,
                },
            ],
            data_groups: vec![],
            navigation_urls: None,
            navigation_request_strategy: None,
        };
        let path = generate_manifest(dist, &config, 0).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let app_urls: Vec<&str> = parsed["assetGroups"][0]["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let fallback_urls: Vec<&str> = parsed["assetGroups"][1]["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(app_urls, vec!["/index.html"]);
        assert!(fallback_urls.is_empty());
    }

    #[test]
    fn manifest_skips_self_and_worker_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let dist = dir.path();
        write(&dist.join("index.html"), "x");
        write(&dist.join("ngsw.json"), "{}"); // stale manifest from prior run
        write(&dist.join("ngsw-worker.js"), "// worker");

        let config = NgswConfig {
            index: Some("/index.html".into()),
            asset_groups: vec![AssetGroup {
                name: "app".into(),
                install_mode: Some("prefetch".into()),
                update_mode: None,
                resources: Resources {
                    files: vec!["/**".into()],
                    urls: vec![],
                },
                cache_query_options: None,
            }],
            data_groups: vec![],
            navigation_urls: None,
            navigation_request_strategy: None,
        };
        generate_manifest(dist, &config, 0).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dist.join("ngsw.json")).unwrap())
                .unwrap();
        let urls: Vec<&str> = parsed["assetGroups"][0]["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(urls, vec!["/index.html"]);
    }

    #[test]
    fn data_groups_compile_to_regex_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let dist = dir.path();
        write(&dist.join("index.html"), "x");
        let config = NgswConfig {
            index: Some("/index.html".into()),
            asset_groups: vec![],
            data_groups: vec![DataGroup {
                name: "api".into(),
                urls: vec!["/api/**".into()],
                version: Some(1),
                cache_config: CacheConfig {
                    max_size: Some(100),
                    max_age: Some("3d".into()),
                    timeout: Some("10s".into()),
                    strategy: Some("freshness".into()),
                },
            }],
            navigation_urls: None,
            navigation_request_strategy: None,
        };
        generate_manifest(dist, &config, 0).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dist.join("ngsw.json")).unwrap())
                .unwrap();
        let dg = &parsed["dataGroups"][0];
        assert_eq!(dg["name"], "api");
        assert_eq!(dg["version"], 1);
        assert_eq!(dg["cacheConfig"]["strategy"], "freshness");
        assert_eq!(dg["cacheConfig"]["maxSize"], 100);
        assert!(dg["patterns"][0].as_str().unwrap().contains("/api"));
    }

    #[test]
    fn default_navigation_urls_include_root_exclude_extensions() {
        let urls = build_navigation_urls(None);
        assert!(urls.iter().any(|u| u.positive));
        assert!(urls.iter().any(|u| !u.positive));
    }

    #[test]
    fn load_config_parses_minimal_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ngsw-config.json");
        std::fs::write(
            &path,
            r#"{
                "index": "/index.html",
                "assetGroups": [
                    { "name": "app", "installMode": "prefetch",
                      "resources": { "files": ["/index.html"] } }
                ]
            }"#,
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.index.as_deref(), Some("/index.html"));
        assert_eq!(config.asset_groups.len(), 1);
        assert_eq!(config.asset_groups[0].name, "app");
    }
}
