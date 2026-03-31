use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};
use colored::Colorize;
use ngc_bundler::{BundleInput, BundleOptions};
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_project_resolver::angular_json::{
    FileReplacement, ResolvedAngularProject, ResolvedAsset, ResolvedStyle,
};

/// Result of the bundled build pipeline.
#[derive(serde::Serialize)]
struct BuildResult {
    /// Number of modules included in the bundle.
    modules_bundled: usize,
    /// Paths to all output files produced.
    output_files: Vec<PathBuf>,
    /// Total size in bytes of all output files.
    total_size_bytes: u64,
}

#[derive(Parser)]
#[command(name = "ngc-rs", about = "Fast Angular project toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print project information: file count, entry points, graph summary.
    Info {
        /// Path to tsconfig.json
        #[arg(long, default_value = "tsconfig.json")]
        project: PathBuf,
    },
    /// Build the project: bundle TypeScript files into a single JavaScript output.
    Build {
        /// Path to tsconfig.json
        #[arg(long, default_value = "tsconfig.json")]
        project: PathBuf,
        /// Output directory (overrides tsconfig/angular.json outDir).
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Build configuration name (e.g. "production", "development").
        #[arg(long, short = 'c')]
        configuration: Option<String>,
        /// Print machine-readable JSON output to stdout.
        #[arg(long)]
        output_json: bool,
    },
}

fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Info { project } => match ngc_project_resolver::resolve_project(&project) {
            Ok(file_graph) => {
                let summary = ngc_project_resolver::summarize(&file_graph);
                println!("{}", "ngc-rs project info".bold());
                println!("  {:<16}{}", "Files:".dimmed(), summary.file_count);
                println!(
                    "  {:<16}{}",
                    "Entry points:".dimmed(),
                    summary.entry_point_count
                );
                println!("  {:<16}{}", "Edges:".dimmed(), summary.edge_count);
                println!(
                    "  {:<16}{}",
                    "Unresolved:".dimmed(),
                    summary.unresolved_count
                );
            }
            Err(e) => {
                eprintln!("{} {e}", "Error:".red().bold());
                process::exit(1);
            }
        },
        Commands::Build {
            project,
            out_dir,
            configuration,
            output_json,
        } => match run_build(&project, out_dir.as_deref(), configuration.as_deref()) {
            Ok(result) => {
                if output_json {
                    let json = serde_json::to_string_pretty(&result)
                        .expect("BuildResult serialization should not fail");
                    println!("{json}");
                } else {
                    println!("{}", "ngc-rs build complete".bold().green());
                    println!("  {:<16}{}", "Bundled:".dimmed(), result.modules_bundled);
                    println!(
                        "  {:<16}{}",
                        "Output files:".dimmed(),
                        result.output_files.len()
                    );
                    println!(
                        "  {:<16}{}",
                        "Total size:".dimmed(),
                        format_bytes(result.total_size_bytes)
                    );
                    for path in &result.output_files {
                        println!("  {:<16}{}", "".dimmed(), path.display());
                    }
                }
            }
            Err(e) => {
                eprintln!("{} {e}", "Error:".red().bold());
                process::exit(1);
            }
        },
    }
}

/// Orchestrate the full build pipeline: resolve → transform → bundle → output.
fn run_build(
    project: &Path,
    out_dir_override: Option<&Path>,
    configuration: Option<&str>,
) -> NgcResult<BuildResult> {
    // Step 1: Try to find angular.json
    let angular_project = find_and_resolve_angular_json(project, configuration)?;

    // Step 2: Determine tsconfig path (angular.json overrides --project)
    let tsconfig_path = angular_project
        .as_ref()
        .map(|ap| ap.ts_config.clone())
        .unwrap_or_else(|| project.to_path_buf());

    let config = ngc_project_resolver::tsconfig::resolve_tsconfig(&tsconfig_path)?;
    let file_graph = ngc_project_resolver::resolve_project(&tsconfig_path)?;

    let config_dir = config
        .config_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    let root_dir = config
        .compiler_options
        .root_dir
        .as_ref()
        .map(|r| config_dir.join(r))
        .unwrap_or_else(|| config_dir.clone());
    let root_dir = root_dir.canonicalize().map_err(|e| NgcError::Io {
        path: root_dir.clone(),
        source: e,
    })?;

    // Step 3: Determine output directory
    let out_dir = out_dir_override
        .map(PathBuf::from)
        .or_else(|| angular_project.as_ref().map(|ap| ap.output_path.clone()))
        .or_else(|| {
            config
                .compiler_options
                .out_dir
                .as_ref()
                .map(|o| config_dir.join(o))
        })
        .unwrap_or_else(|| config_dir.join("dist"));

    // Step 4: Compile templates
    let files: Vec<PathBuf> = file_graph.graph.node_weights().cloned().collect();
    let compiled = ngc_template_compiler::compile_templates(&files)?;

    // Report any JIT fallbacks
    for cf in &compiled {
        if cf.jit_fallback {
            eprintln!(
                "{} JIT fallback for {}",
                "Warning:".yellow().bold(),
                cf.source_path.display()
            );
        }
    }

    // Step 5: Apply fileReplacements
    let sources: Vec<(PathBuf, String)> = compiled
        .into_iter()
        .map(|cf| (cf.source_path, cf.source))
        .collect();
    let file_replacements = angular_project
        .as_ref()
        .map(|ap| ap.file_replacements.as_slice())
        .unwrap_or(&[]);
    let sources = apply_file_replacements(sources, file_replacements, &config_dir)?;

    // Step 6: Transform TS → JS
    let bundle_options = build_options(configuration);
    let transformed = transform_with_fallback(&sources, bundle_options.source_maps)?;

    // Build modules map (canonical source path → JS code) and collect source maps
    let mut modules: HashMap<PathBuf, String> = HashMap::new();
    let mut per_module_maps: HashMap<PathBuf, oxc_sourcemap::SourceMap> = HashMap::new();
    for m in transformed {
        if let Some(map) = m.source_map {
            per_module_maps.insert(m.source_path.clone(), map);
        }
        modules.insert(m.source_path, m.code);
    }

    // Find the entry point (look for main.ts among graph entry points)
    let entry = find_entry_point(&file_graph.entry_points)?;

    // Derive local prefixes from tsconfig path aliases
    let mut local_prefixes = vec![".".to_string()];
    if let Some(paths) = &config.compiler_options.paths {
        for alias in paths.keys() {
            if let Some(prefix) = alias.strip_suffix('*') {
                local_prefixes.push(prefix.to_string());
            }
        }
    }

    // Step 6.5: Resolve npm dependencies
    // Collect bare specifiers from project scanning AND from transformed output
    // (oxc may inject new imports like @oxc-project/runtime/helpers/decorate)
    let mut bare_specifiers: Vec<String> = file_graph.npm_import_sites.keys().cloned().collect();
    let post_transform_specifiers = scan_transformed_bare_specifiers(&modules, &local_prefixes);
    for spec in post_transform_specifiers {
        if !bare_specifiers.contains(&spec) {
            bare_specifiers.push(spec);
        }
    }
    let npm_resolution = ngc_npm_resolver::resolve_npm_dependencies(&bare_specifiers, &config_dir)?;

    // Merge npm modules into the modules map (they're already JS — no transform needed)
    for (path, source) in &npm_resolution.modules {
        modules.insert(path.clone(), source.clone());
    }

    // Step 6.6: Link partially compiled Angular npm packages
    let linker_stats = ngc_linker::link_npm_modules(&mut modules, &config_dir)?;
    if linker_stats.files_linked > 0 {
        tracing::info!(
            "linked {} Angular package file(s)",
            linker_stats.files_linked
        );
    }

    // Inject vendored helpers for oxc runtime (not an npm dependency of the project)
    let injected_helpers = inject_oxc_runtime_helpers(&mut modules, &bare_specifiers, &config_dir);

    // Add npm file nodes and injected helper nodes to the graph
    let mut graph = file_graph.graph;
    let mut path_index = file_graph.path_index;
    for path in npm_resolution.modules.keys() {
        if !path_index.contains_key(path) {
            let idx = graph.add_node(path.clone());
            path_index.insert(path.clone(), idx);
        }
    }
    for (_, helper_path) in &injected_helpers {
        if !path_index.contains_key(helper_path) {
            let idx = graph.add_node(helper_path.clone());
            path_index.insert(helper_path.clone(), idx);
        }
    }

    // Add edges from project files to npm entry files
    for (specifier, import_sites) in &file_graph.npm_import_sites {
        if let Some(entry_path) = npm_resolution
            .resolved_specifiers
            .contains(specifier)
            .then(|| ngc_npm_resolver::resolve::resolve_bare_specifier(specifier, &config_dir).ok())
            .flatten()
        {
            if let Some(&to_idx) = path_index.get(&entry_path) {
                for (from_file, kind) in import_sites {
                    if let Some(&from_idx) = path_index.get(from_file) {
                        graph.add_edge(from_idx, to_idx, *kind);
                    }
                }
            }
        }
    }

    // Add internal npm edges
    for (from, to, kind) in &npm_resolution.edges {
        if let (Some(&from_idx), Some(&to_idx)) = (path_index.get(from), path_index.get(to)) {
            graph.add_edge(from_idx, to_idx, *kind);
        }
    }

    // Add injected helpers to resolved specifiers and connect dependency edges.
    // We connect every project file that imports the helper (not just the entry point)
    // so topological ordering places the helper before all files that use it.
    let mut bundled_specifiers = npm_resolution.resolved_specifiers;
    for (spec, helper_path) in &injected_helpers {
        bundled_specifiers.insert(spec.clone());
        if let Some(&to_idx) = path_index.get(helper_path) {
            // Find all project modules that import this specifier
            for (module_path, module_source) in &modules {
                if module_path
                    .components()
                    .any(|c| c.as_os_str() == "node_modules")
                {
                    continue;
                }
                if module_source.contains(spec.as_str()) {
                    if let Some(&from_idx) = path_index.get(module_path) {
                        graph.add_edge(from_idx, to_idx, ngc_project_resolver::ImportKind::Static);
                    }
                }
            }
        }
    }

    let bundle_input = BundleInput {
        modules,
        graph,
        entry,
        local_prefixes,
        root_dir,
        options: bundle_options,
        per_module_maps,
        bundled_specifiers,
    };

    let bundle_output = ngc_bundler::bundle(&bundle_input)?;
    let modules_bundled: usize = bundle_output
        .chunks
        .values()
        .map(|code| code.matches("\n// ").count() + 1)
        .sum();

    // Step 7: Write outputs
    std::fs::create_dir_all(&out_dir).map_err(|e| NgcError::Io {
        path: out_dir.clone(),
        source: e,
    })?;

    let mut output_files: Vec<PathBuf> = Vec::new();

    // Write all chunk files (main.js + chunk-*.js) with optional source maps
    for (filename, code) in &bundle_output.chunks {
        let mut final_code = code.clone();

        // Append source map reference if we have a map for this chunk
        if let Some(source_map) = bundle_output.chunk_source_maps.get(filename) {
            if bundle_options.source_maps {
                if configuration == Some("production") {
                    // External source map file
                    let map_filename = format!("{filename}.map");
                    final_code.push_str(&format!("//# sourceMappingURL={map_filename}\n"));
                    let map_path = out_dir.join(&map_filename);
                    std::fs::write(&map_path, source_map.to_json_string()).map_err(|e| {
                        NgcError::Io {
                            path: map_path.clone(),
                            source: e,
                        }
                    })?;
                    output_files.push(map_path);
                } else {
                    // Inline source map (data URL)
                    final_code.push_str(&format!(
                        "//# sourceMappingURL={}\n",
                        source_map.to_data_url()
                    ));
                }
            }
        }

        let path = out_dir.join(filename);
        std::fs::write(&path, &final_code).map_err(|e| NgcError::Io {
            path: path.clone(),
            source: e,
        })?;
        output_files.push(path);
    }

    // Step 8: Generate polyfills.js
    if let Some(ref ap) = angular_project {
        if !ap.polyfills.is_empty() {
            let path = generate_polyfills(&ap.polyfills, &out_dir)?;
            output_files.push(path);
        }
    }

    // Step 9: Extract global styles
    if let Some(ref ap) = angular_project {
        if !ap.styles.is_empty() {
            let path = extract_global_styles(&ap.styles, &out_dir, &config_dir)?;
            output_files.push(path);
        }
    }

    // Step 10: Copy assets
    if let Some(ref ap) = angular_project {
        if !ap.assets.is_empty() {
            let paths = copy_assets(&ap.assets, &out_dir)?;
            output_files.extend(paths);
        }
    }

    // Step 11: Generate index.html
    if let Some(ref ap) = angular_project {
        if let Some(ref index_path) = ap.index_html {
            let path = generate_index_html(
                index_path,
                &ap.index_output,
                !ap.styles.is_empty(),
                !ap.polyfills.is_empty(),
                &out_dir,
                &bundle_output.main_filename,
            )?;
            output_files.push(path);
        }
    }

    // Step 12: Generate 3rdpartylicenses.txt
    let all_bundle_code: String = bundle_output
        .chunks
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(lp) = generate_third_party_licenses(&all_bundle_code, &config_dir, &out_dir)? {
        output_files.push(lp);
    }

    // Compute total size
    let total_size_bytes = output_files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    Ok(BuildResult {
        modules_bundled,
        output_files,
        total_size_bytes,
    })
}

/// Derive bundle options from the build configuration name.
///
/// Production enables all optimizations (source maps, minification, tree shaking,
/// content hashing). Development and unspecified configurations use defaults
/// (all optimizations disabled).
fn build_options(configuration: Option<&str>) -> BundleOptions {
    match configuration {
        Some("production") => BundleOptions {
            source_maps: true,
            minify: true,
            content_hash: true,
            // Tree shaking disabled: conflicts with dynamic import rewrites
            // when an export declaration contains import() expressions.
            // TODO: fix tree shaker to skip declarations with nested dynamic imports
            tree_shake: false,
        },
        _ => BundleOptions::default(),
    }
}

/// Try to find angular.json by searching upward from the project file's directory.
fn find_and_resolve_angular_json(
    project: &Path,
    configuration: Option<&str>,
) -> NgcResult<Option<ResolvedAngularProject>> {
    let parent = project.parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let start_dir = parent.canonicalize().map_err(|e| NgcError::Io {
        path: project.to_path_buf(),
        source: e,
    })?;

    let mut dir = start_dir.as_path();
    loop {
        let candidate = dir.join("angular.json");
        if candidate.exists() {
            let resolved = ngc_project_resolver::angular_json::resolve_angular_project(
                &candidate,
                None,
                configuration,
            )?;
            return Ok(Some(resolved));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(None),
        }
    }
}

/// Apply file replacements to source content.
///
/// For each replacement, if a source path matches the `replace` path,
/// read the `with` file and substitute its content. The path key stays
/// the same so the bundler sees the original module identity.
fn apply_file_replacements(
    sources: Vec<(PathBuf, String)>,
    replacements: &[FileReplacement],
    base_dir: &Path,
) -> NgcResult<Vec<(PathBuf, String)>> {
    if replacements.is_empty() {
        return Ok(sources);
    }

    // Pre-resolve replacement paths
    let resolved_replacements: Vec<(PathBuf, PathBuf)> = replacements
        .iter()
        .filter_map(|fr| {
            let replace_path = base_dir.join(&fr.replace).canonicalize().ok()?;
            let with_path = base_dir.join(&fr.with_file).canonicalize().ok()?;
            Some((replace_path, with_path))
        })
        .collect();

    sources
        .into_iter()
        .map(|(path, source)| {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            for (replace_path, with_path) in &resolved_replacements {
                if canonical == *replace_path {
                    let replacement_content =
                        std::fs::read_to_string(with_path).map_err(|e| NgcError::Io {
                            path: with_path.clone(),
                            source: e,
                        })?;
                    return Ok((path, replacement_content));
                }
            }
            Ok((path, source))
        })
        .collect()
}

/// Generate dist/polyfills.js with import statements for each polyfill entry.
fn generate_polyfills(polyfills: &[String], out_dir: &Path) -> NgcResult<PathBuf> {
    let content: String = polyfills
        .iter()
        .map(|p| format!("import '{p}';\n"))
        .collect();
    let path = out_dir.join("polyfills.js");
    std::fs::write(&path, &content).map_err(|e| NgcError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// Read and concatenate global CSS style files, writing dist/styles.css.
///
/// Resolves CSS `@import` directives that reference npm packages (e.g.
/// `@import "tailwindcss"`) by looking up the package in `node_modules`.
fn extract_global_styles(
    styles: &[ResolvedStyle],
    out_dir: &Path,
    project_root: &Path,
) -> NgcResult<PathBuf> {
    let mut css = String::new();
    for style in styles {
        if !style.inject {
            continue;
        }
        let ext = style
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if ext != "css" {
            return Err(NgcError::StyleError {
                path: style.path.clone(),
                message: format!(".{ext} files are not supported — only plain .css in v0.5"),
            });
        }
        let content = std::fs::read_to_string(&style.path).map_err(|e| NgcError::Io {
            path: style.path.clone(),
            source: e,
        })?;
        if !css.is_empty() {
            css.push('\n');
        }
        let resolved = resolve_css_imports(&content, project_root);
        css.push_str(&resolved);
    }
    let path = out_dir.join("styles.css");
    std::fs::write(&path, &css).map_err(|e| NgcError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// Resolve CSS `@import` directives that reference npm packages.
///
/// Replaces `@import "package"` and `@import "package/file"` with the inlined
/// contents of the resolved CSS file from `node_modules`. Lines that reference
/// local files or URLs are left unchanged. Non-CSS `@import` directives
/// (e.g. `@import "tailwindcss"`) that resolve to a CSS file are also inlined.
fn resolve_css_imports(css: &str, project_root: &Path) -> String {
    let node_modules = project_root.join("node_modules");
    let mut result = String::new();

    for line in css.lines() {
        let trimmed = line.trim();
        if let Some(specifier) = extract_css_import_specifier(trimmed) {
            // Skip URLs and relative paths
            if specifier.starts_with("http")
                || specifier.starts_with("//")
                || specifier.starts_with('.')
            {
                result.push_str(line);
                result.push('\n');
                continue;
            }

            // Try to resolve from node_modules
            if let Some(resolved_content) = resolve_npm_css(&node_modules, &specifier, project_root)
            {
                result.push_str(&format!("/* @import \"{specifier}\" (resolved) */\n"));
                result.push_str(&resolved_content);
                result.push('\n');
                continue;
            }
        }

        // Lines starting with @config or other directives we can't resolve: skip them
        if trimmed.starts_with("@config") {
            result.push_str(&format!("/* {trimmed} (skipped) */\n"));
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    result
}

/// Extract the specifier from a CSS `@import` directive.
fn extract_css_import_specifier(line: &str) -> Option<String> {
    if !line.starts_with("@import") {
        return None;
    }
    // @import "specifier"; or @import 'specifier'; or @import url("specifier");
    let rest = line.strip_prefix("@import")?.trim();
    let rest = rest.strip_suffix(';').unwrap_or(rest).trim();

    if let Some(inner) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(inner.to_string());
    }
    if let Some(inner) = rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Some(inner.to_string());
    }
    // Bare specifier without quotes (e.g. @import tailwindcss;)
    if !rest.is_empty() && !rest.starts_with("url(") && !rest.contains(' ') && !rest.contains('(') {
        return Some(rest.to_string());
    }

    None
}

/// Try to resolve a CSS file from node_modules.
fn resolve_npm_css(node_modules: &Path, specifier: &str, project_root: &Path) -> Option<String> {
    // Try direct path: node_modules/{specifier}
    let direct = node_modules.join(specifier);

    // Try with .css extension
    let candidates = [
        direct.clone(),
        direct.with_extension("css"),
        direct.join("index.css"),
    ];

    for candidate in &candidates {
        if candidate.is_file() {
            return std::fs::read_to_string(candidate).ok();
        }
    }

    // Try resolving via the package's package.json "style" or "main" field
    let pkg_name = if specifier.starts_with('@') {
        // Scoped package: @scope/pkg or @scope/pkg/file
        specifier
            .splitn(3, '/')
            .take(2)
            .collect::<Vec<_>>()
            .join("/")
    } else {
        specifier.split('/').next().unwrap_or(specifier).to_string()
    };

    let pkg_json_path = node_modules.join(&pkg_name).join("package.json");
    if let Ok(pkg_json) = std::fs::read_to_string(&pkg_json_path) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&pkg_json) {
            // Check "style" field first, then "exports" for CSS
            if let Some(style) = pkg.get("style").and_then(|v| v.as_str()) {
                let style_path = node_modules.join(&pkg_name).join(style);
                if style_path.is_file() {
                    return std::fs::read_to_string(&style_path).ok();
                }
            }

            // Check exports for CSS
            if let Some(exports) = pkg.get("exports") {
                if let Some(css_path) = find_css_in_exports(exports, specifier, &pkg_name) {
                    let full_path = node_modules.join(&pkg_name).join(css_path);
                    if full_path.is_file() {
                        return std::fs::read_to_string(&full_path).ok();
                    }
                }
            }
        }
    }

    // Last resort for bare package names: check if the package itself is a CSS framework
    // (e.g. "tailwindcss" ships a preflight/base CSS)
    let base_css = node_modules.join(&pkg_name).join("theme.css");
    if base_css.is_file() {
        return std::fs::read_to_string(&base_css).ok();
    }

    // For packages like tailwindcss that are build-time only, return an empty comment
    let pkg_dir = node_modules.join(&pkg_name);
    if pkg_dir.is_dir() {
        // Package exists but has no resolvable CSS — it's likely a build-time tool
        eprintln!(
            "{} CSS @import \"{specifier}\" skipped (no CSS entry point found in package)",
            "Warning:".yellow().bold()
        );
        return Some(format!(
            "/* @import \"{specifier}\" — build-time only, skipped */"
        ));
    }

    // Check if it's a subpath like "ngx-toastr/toastr" → node_modules/ngx-toastr/toastr.css
    let subpath = node_modules.join(specifier.replace('/', std::path::MAIN_SEPARATOR_STR));
    let subpath_css = subpath.with_extension("css");
    if subpath_css.is_file() {
        return std::fs::read_to_string(&subpath_css).ok();
    }

    // Also try node_modules relative from project root
    let alt = project_root.join("node_modules").join(specifier);
    let alt_css = alt.with_extension("css");
    if alt_css.is_file() {
        return std::fs::read_to_string(&alt_css).ok();
    }

    None
}

/// Copy asset files and directories to the output directory.
fn copy_assets(assets: &[ResolvedAsset], out_dir: &Path) -> NgcResult<Vec<PathBuf>> {
    let mut output_paths = Vec::new();
    for asset in assets {
        match asset {
            ResolvedAsset::Path(src) => {
                if src.is_dir() {
                    let dir_name = src.file_name().unwrap_or_default();
                    let dst = out_dir.join(dir_name);
                    let paths = copy_dir_recursive(src, &dst)?;
                    output_paths.extend(paths);
                } else if src.is_file() {
                    let file_name = src.file_name().unwrap_or_default();
                    let dst = out_dir.join(file_name);
                    std::fs::copy(src, &dst).map_err(|e| NgcError::AssetError {
                        path: src.clone(),
                        message: format!("failed to copy: {e}"),
                    })?;
                    output_paths.push(dst);
                }
                // Skip non-existent paths silently (e.g. src/favicon.ico)
            }
            ResolvedAsset::Glob {
                pattern,
                input,
                output,
                ignore: _,
            } => {
                let glob_pattern = format!("{}/{pattern}", input.display());
                let entries = glob::glob(&glob_pattern).map_err(|e| NgcError::AssetError {
                    path: input.clone(),
                    message: format!("invalid glob pattern: {e}"),
                })?;
                let output_base = out_dir.join(output.trim_start_matches('/'));
                for entry in entries {
                    let entry = entry.map_err(|e| NgcError::AssetError {
                        path: input.clone(),
                        message: format!("glob error: {e}"),
                    })?;
                    if entry.is_file() {
                        let relative = entry.strip_prefix(input).unwrap_or(&entry).to_path_buf();
                        let dst = output_base.join(&relative);
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent).map_err(|e| NgcError::Io {
                                path: parent.to_path_buf(),
                                source: e,
                            })?;
                        }
                        std::fs::copy(&entry, &dst).map_err(|e| NgcError::AssetError {
                            path: entry.clone(),
                            message: format!("failed to copy: {e}"),
                        })?;
                        output_paths.push(dst);
                    }
                }
            }
        }
    }
    Ok(output_paths)
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> NgcResult<Vec<PathBuf>> {
    let mut output_paths = Vec::new();
    std::fs::create_dir_all(dst).map_err(|e| NgcError::Io {
        path: dst.to_path_buf(),
        source: e,
    })?;
    let entries = std::fs::read_dir(src).map_err(|e| NgcError::Io {
        path: src.to_path_buf(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| NgcError::Io {
            path: src.to_path_buf(),
            source: e,
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            let paths = copy_dir_recursive(&src_path, &dst_path)?;
            output_paths.extend(paths);
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| NgcError::AssetError {
                path: src_path,
                message: format!("failed to copy: {e}"),
            })?;
            output_paths.push(dst_path);
        }
    }
    Ok(output_paths)
}

/// Read source index.html, inject stylesheet and script tags, write to out_dir.
fn generate_index_html(
    index_source: &Path,
    output_filename: &str,
    has_styles: bool,
    has_polyfills: bool,
    out_dir: &Path,
    main_filename: &str,
) -> NgcResult<PathBuf> {
    let mut html = std::fs::read_to_string(index_source).map_err(|e| NgcError::Io {
        path: index_source.to_path_buf(),
        source: e,
    })?;

    // Inject stylesheet link before </head>
    if has_styles {
        html = html.replace(
            "</head>",
            "  <link rel=\"stylesheet\" href=\"styles.css\">\n</head>",
        );
    }

    // Inject script tags before </body>
    let mut scripts = String::new();
    if has_polyfills {
        scripts.push_str("  <script src=\"polyfills.js\" type=\"module\"></script>\n");
    }
    scripts.push_str(&format!(
        "  <script src=\"{main_filename}\" type=\"module\"></script>\n"
    ));
    html = html.replace("</body>", &format!("{scripts}</body>"));

    let path = out_dir.join(output_filename);
    std::fs::write(&path, &html).map_err(|e| NgcError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// Scan the bundle's external imports, find LICENSE files in node_modules,
/// and concatenate them into 3rdpartylicenses.txt.
fn generate_third_party_licenses(
    bundle_code: &str,
    project_root: &Path,
    out_dir: &Path,
) -> NgcResult<Option<PathBuf>> {
    let node_modules = project_root.join("node_modules");
    if !node_modules.is_dir() {
        return Ok(None);
    }

    // Extract package names from external imports
    let import_re =
        regex::Regex::new(r#"import\s+.*?from\s+['"]([@\w][\w./-]*?)['"]"#).map_err(|e| {
            NgcError::BundleError {
                message: format!("regex error: {e}"),
            }
        })?;

    let mut packages = BTreeSet::new();
    for cap in import_re.captures_iter(bundle_code) {
        let specifier = &cap[1];
        // Extract package name: @scope/pkg or pkg
        let pkg_name = if specifier.starts_with('@') {
            specifier
                .splitn(3, '/')
                .take(2)
                .collect::<Vec<_>>()
                .join("/")
        } else {
            specifier.split('/').next().unwrap_or(specifier).to_string()
        };
        packages.insert(pkg_name);
    }

    if packages.is_empty() {
        return Ok(None);
    }

    let license_filenames = ["LICENSE", "LICENSE.md", "LICENSE.txt", "LICENCE", "license"];
    let mut content = String::new();

    for pkg in &packages {
        let pkg_dir = node_modules.join(pkg);
        if !pkg_dir.is_dir() {
            continue;
        }
        for filename in &license_filenames {
            let license_path = pkg_dir.join(filename);
            if license_path.is_file() {
                if let Ok(text) = std::fs::read_to_string(&license_path) {
                    content.push_str(pkg);
                    content.push('\n');
                    content.push_str(&text);
                    content.push_str("\n\n");
                }
                break;
            }
        }
    }

    if content.is_empty() {
        return Ok(None);
    }

    let path = out_dir.join("3rdpartylicenses.txt");
    std::fs::write(&path, &content).map_err(|e| NgcError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(Some(path))
}

/// Search package.json "exports" for a CSS file path.
fn find_css_in_exports(
    exports: &serde_json::Value,
    _specifier: &str,
    _pkg_name: &str,
) -> Option<String> {
    // Handle simple string export
    if let Some(s) = exports.as_str() {
        if s.ends_with(".css") {
            return Some(s.to_string());
        }
    }

    // Handle object exports with "style" or "default" keys
    if let Some(obj) = exports.as_object() {
        if let Some(style) = obj.get("style").and_then(|v| v.as_str()) {
            if style.ends_with(".css") {
                return Some(style.to_string());
            }
        }
        // Recurse into "." entry
        if let Some(dot) = obj.get(".") {
            return find_css_in_exports(dot, _specifier, _pkg_name);
        }
    }

    None
}

/// Format byte count as human-readable string.
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Transform sources with fallback: if a compiled source fails oxc parsing,
/// re-read the original file and transform that instead.
fn transform_with_fallback(
    sources: &[(PathBuf, String)],
    generate_source_maps: bool,
) -> NgcResult<Vec<ngc_ts_transform::TransformedModule>> {
    let results: Vec<NgcResult<ngc_ts_transform::TransformedModule>> = sources
        .iter()
        .map(|(path, source)| {
            let file_name = path.to_string_lossy();
            match ngc_ts_transform::transform_source_with_map(
                source,
                &file_name,
                generate_source_maps,
            ) {
                Ok((code, source_map)) => Ok(ngc_ts_transform::TransformedModule {
                    source_path: path.clone(),
                    code,
                    source_map,
                }),
                Err(e) => {
                    eprintln!(
                        "{} transform fallback for {} ({})",
                        "Warning:".yellow().bold(),
                        path.display(),
                        e
                    );
                    let original = std::fs::read_to_string(path).map_err(|e| NgcError::Io {
                        path: path.clone(),
                        source: e,
                    })?;
                    let (code, source_map) = ngc_ts_transform::transform_source_with_map(
                        &original,
                        &file_name,
                        generate_source_maps,
                    )?;
                    Ok(ngc_ts_transform::TransformedModule {
                        source_path: path.clone(),
                        code,
                        source_map,
                    })
                }
            }
        })
        .collect();

    results.into_iter().collect()
}

/// Vendored oxc runtime helpers.
///
/// When the oxc transformer injects `import _decorate from '@oxc-project/runtime/helpers/decorate'`,
/// this helper is usually not installed as a project dependency. We inline the helper code
/// directly so it gets bundled without requiring `npm install @oxc-project/runtime`.
const OXC_DECORATE_HELPER: &str = r#"function __decorate(decorators, target, key, desc) {
  var c = arguments.length,
    r = c < 3 ? target : desc === null ? (desc = Object.getOwnPropertyDescriptor(target, key)) : desc,
    d;
  if (typeof Reflect === "object" && typeof Reflect.decorate === "function")
    r = Reflect.decorate(decorators, target, key, desc);
  else
    for (var i = decorators.length - 1; i >= 0; i--)
      if ((d = decorators[i]))
        r = (c < 3 ? d(r) : c > 3 ? d(target, key, r) : d(target, key)) || r;
  return c > 3 && r && Object.defineProperty(target, key, r), r;
}
export { __decorate as default };
"#;

/// Inject vendored oxc runtime helpers into the modules map.
///
/// If the transformed code references `@oxc-project/runtime/helpers/decorate` and
/// the package is not installed in `node_modules`, we inject a vendored copy of
/// the helper so it can be bundled without requiring an npm install.
/// Inject vendored oxc runtime helpers into the modules map.
///
/// If the transformed code references `@oxc-project/runtime/helpers/decorate` and
/// the package is not installed in `node_modules`, we inject a vendored copy of
/// the helper so it can be bundled without requiring an npm install.
/// Returns the specifiers that were injected (to add to `resolved_specifiers`).
fn inject_oxc_runtime_helpers(
    modules: &mut HashMap<PathBuf, String>,
    bare_specifiers: &[String],
    project_root: &Path,
) -> Vec<(String, PathBuf)> {
    let runtime_helpers = [("@oxc-project/runtime/helpers/decorate", OXC_DECORATE_HELPER)];
    let mut injected = Vec::new();

    for (specifier, helper_code) in &runtime_helpers {
        let spec_str = specifier.to_string();
        if !bare_specifiers.contains(&spec_str) {
            continue;
        }
        // Only inject if the package is not already installed
        let pkg_dir = project_root.join("node_modules/@oxc-project/runtime");
        if pkg_dir.is_dir() {
            continue;
        }
        // Create a synthetic file path for the vendored helper
        let synthetic_path = project_root
            .join("node_modules")
            .join(specifier.replace('/', std::path::MAIN_SEPARATOR_STR))
            .with_extension("js");

        modules.insert(synthetic_path.clone(), helper_code.to_string());
        injected.push((spec_str, synthetic_path));
    }

    injected
}

/// Scan transformed JS code for bare import specifiers not matching local prefixes.
///
/// This catches imports injected by the TS transformer (e.g. `@oxc-project/runtime`)
/// that weren't present in the original TypeScript source code.
fn scan_transformed_bare_specifiers(
    modules: &HashMap<PathBuf, String>,
    local_prefixes: &[String],
) -> Vec<String> {
    let import_re = regex::Regex::new(r#"(?:import|export)\s+.*?\s+from\s+['"]([^'"]+)['"]"#)
        .expect("valid regex");
    let mut specifiers = std::collections::HashSet::new();

    for code in modules.values() {
        for cap in import_re.captures_iter(code) {
            let spec = &cap[1];
            // Skip relative and local-prefix imports
            if spec.starts_with('.')
                || local_prefixes
                    .iter()
                    .any(|prefix| spec.starts_with(prefix.as_str()))
            {
                continue;
            }
            specifiers.insert(spec.to_string());
        }
    }

    specifiers.into_iter().collect()
}

/// Find the entry point from graph entry points by looking for main.ts.
fn find_entry_point(entry_points: &[PathBuf]) -> NgcResult<PathBuf> {
    entry_points
        .iter()
        .find(|p| {
            p.file_name()
                .is_some_and(|name| name == "main.ts" || name == "main.tsx")
        })
        .cloned()
        .ok_or_else(|| NgcError::BundleError {
            message: format!(
                "no main.ts entry point found among candidates: {:?}",
                entry_points
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1048576), "1.0 MB");
    }

    #[test]
    fn test_generate_polyfills_content() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = generate_polyfills(&["zone.js".to_string()], dir.path()).unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(content, "import 'zone.js';\n");
    }

    #[test]
    fn test_generate_polyfills_multiple() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = generate_polyfills(
            &["zone.js".to_string(), "zone.js/testing".to_string()],
            dir.path(),
        )
        .unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(content, "import 'zone.js';\nimport 'zone.js/testing';\n");
    }

    #[test]
    fn test_index_html_injection() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let index_src = dir.path().join("index.html");
        std::fs::write(
            &index_src,
            "<!doctype html>\n<html>\n<head>\n</head>\n<body>\n  <app-root></app-root>\n</body>\n</html>\n",
        )
        .unwrap();
        let out = generate_index_html(&index_src, "index.html", true, true, dir.path(), "main.js")
            .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(content.contains(r#"<link rel="stylesheet" href="styles.css">"#));
        assert!(content.contains(r#"<script src="polyfills.js" type="module"></script>"#));
        assert!(content.contains(r#"<script src="main.js" type="module"></script>"#));
    }

    #[test]
    fn test_index_html_no_styles_no_polyfills() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let index_src = dir.path().join("index.html");
        std::fs::write(&index_src, "<html><head></head><body></body></html>").unwrap();
        let out = generate_index_html(
            &index_src,
            "index.html",
            false,
            false,
            dir.path(),
            "main.js",
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(!content.contains("styles.css"));
        assert!(!content.contains("polyfills.js"));
        assert!(content.contains(r#"<script src="main.js" type="module"></script>"#));
    }

    #[test]
    fn test_build_options_production() {
        let opts = build_options(Some("production"));
        assert!(opts.source_maps);
        assert!(opts.minify);
        assert!(opts.content_hash);
        assert!(!opts.tree_shake); // disabled: conflicts with dynamic import rewrites
    }

    #[test]
    fn test_build_options_development() {
        let opts = build_options(Some("development"));
        assert!(!opts.source_maps);
        assert!(!opts.minify);
        assert!(!opts.content_hash);
        assert!(!opts.tree_shake);
    }

    #[test]
    fn test_build_options_none() {
        let opts = build_options(None);
        assert!(!opts.source_maps);
        assert!(!opts.minify);
        assert!(!opts.content_hash);
        assert!(!opts.tree_shake);
    }

    #[test]
    fn test_apply_file_replacements_swaps_content() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let env_file = dir.path().join("env.ts");
        let env_prod = dir.path().join("env.prod.ts");
        std::fs::write(&env_file, "const prod = false;").unwrap();
        std::fs::write(&env_prod, "const prod = true;").unwrap();

        let sources = vec![(env_file.clone(), "const prod = false;".to_string())];
        let replacements = vec![FileReplacement {
            replace: "env.ts".to_string(),
            with_file: "env.prod.ts".to_string(),
        }];

        let result = apply_file_replacements(sources, &replacements, dir.path()).unwrap();
        assert_eq!(result[0].1, "const prod = true;");
        // Path key should remain the original
        assert_eq!(result[0].0, env_file);
    }
}
