use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};
use colored::Colorize;
use ngc_bundler::{BundleInput, BundleOptions};
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_project_resolver::angular_json::{
    CrossOrigin, FileReplacement, I18nConfig, InlineStyleLanguage, ResolvedAngularProject,
    ResolvedAsset, ResolvedStyle,
};
use ngc_template_compiler::{StyleContext, StyleLanguage};

mod localize;
mod ngsw;
mod polyfills;

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
        /// Emit one `<out_dir>/<locale>/` tree per locale defined in
        /// `angular.json`'s `i18n.locales` block, applying translations to
        /// every `$localize\`...\`` literal in the bundled output. The
        /// source-locale build is moved under
        /// `<out_dir>/<sourceLocale>/`.
        #[arg(long)]
        localize: bool,
    },
    /// Extract translatable messages from every component template in the
    /// project and emit a `messages.xlf` (XLIFF 1.2) file.
    ExtractI18n {
        /// Path to tsconfig.json
        #[arg(long, default_value = "tsconfig.json")]
        project: PathBuf,
        /// Output file path (defaults to `messages.xlf` in the project dir).
        #[arg(long, short = 'o')]
        out_file: Option<PathBuf>,
        /// Source locale recorded in the XLIFF `source-language` attribute.
        /// Defaults to the value from `angular.json` or `"en-US"`.
        #[arg(long)]
        source_locale: Option<String>,
    },
}

fn main() {
    init_tracing();
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
        Commands::ExtractI18n {
            project,
            out_file,
            source_locale,
        } => match run_extract_i18n(&project, out_file.as_deref(), source_locale.as_deref()) {
            Ok(report) => {
                println!("{}", "ngc-rs i18n extraction complete".bold().green());
                println!("  {:<16}{}", "Messages:".dimmed(), report.message_count);
                println!("  {:<16}{}", "Output:".dimmed(), report.out_file.display());
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
            localize,
        } => match run_build(
            &project,
            out_dir.as_deref(),
            configuration.as_deref(),
            localize,
        ) {
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

/// Configure the global tracing subscriber.
///
/// Honours `RUST_LOG` (e.g. `RUST_LOG=info`) and emits span-close events so
/// `info_span!("stage")` sections report their elapsed time. With no env var
/// set the output is silent, matching the prior behaviour.
fn init_tracing() {
    use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Orchestrate the full build pipeline: resolve → transform → bundle → output.
fn run_build(
    project: &Path,
    out_dir_override: Option<&Path>,
    configuration: Option<&str>,
    localize: bool,
) -> NgcResult<BuildResult> {
    // Step 1: Try to find angular.json
    let angular_project = find_and_resolve_angular_json(project, configuration)?;

    // Step 2: Determine tsconfig path (angular.json overrides --project)
    let tsconfig_path = angular_project
        .as_ref()
        .map(|ap| ap.ts_config.clone())
        .unwrap_or_else(|| project.to_path_buf());

    let resolve_span = tracing::info_span!("resolve").entered();
    let config = ngc_project_resolver::tsconfig::resolve_tsconfig(&tsconfig_path)?;
    let file_graph = ngc_project_resolver::resolve_project(&tsconfig_path)?;
    drop(resolve_span);

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

    // Step 3.5: Start CSS/PostCSS work concurrently with the rest of the
    // pipeline. The PostCSS subprocess (Node + Tailwind) takes ~200 ms of
    // wall time but shares no state with the bundler, so spawning it here
    // and awaiting it during io_outputs overlaps the cost.
    let css_job = {
        let _span = tracing::info_span!("css_spawn").entered();
        start_css_job(angular_project.as_ref(), &out_dir, &config_dir)?
    };

    // Step 4: Compile Angular decorators (@Component, @Injectable, @Directive, @Pipe, @NgModule)
    let templates_span = tracing::info_span!("template_compile").entered();
    let files: Vec<PathBuf> = file_graph.graph.node_weights().cloned().collect();
    let style_ctx = build_style_context(angular_project.as_ref(), &config_dir);
    let compiled = ngc_template_compiler::compile_all_decorators_with_styles(&files, &style_ctx)?;
    drop(templates_span);

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
    let transform_span = tracing::info_span!("ts_transform").entered();
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
    drop(transform_span);

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
    let npm_span = tracing::info_span!("npm_resolve").entered();
    let mut bare_specifiers: Vec<String> = file_graph.npm_import_sites.keys().cloned().collect();
    let post_transform_specifiers = scan_transformed_bare_specifiers(&modules, &local_prefixes);
    for spec in post_transform_specifiers {
        if !bare_specifiers.contains(&spec) {
            bare_specifiers.push(spec);
        }
    }
    let export_conditions =
        ngc_npm_resolver::package_json::conditions_for_configuration(configuration);
    let mut npm_resolution = ngc_npm_resolver::resolve_npm_dependencies(
        &bare_specifiers,
        &config_dir,
        export_conditions,
    )?;

    // Merge npm modules into the modules map (they're already JS — no transform needed)
    for (path, source) in &npm_resolution.modules {
        modules.insert(path.clone(), source.clone());
    }
    drop(npm_span);

    // Step 6.6: Link partially compiled Angular npm packages and flatten
    // NgModule references in component dependencies arrays.
    let link_span = tracing::info_span!("link").entered();
    let linker_stats = ngc_linker::link_modules(&mut modules, &config_dir)?;
    if linker_stats.files_linked > 0 {
        tracing::info!(
            "linked {} Angular package file(s)",
            linker_stats.files_linked
        );
    }
    if linker_stats.components_flattened > 0 {
        tracing::info!(
            "flattened NgModule imports in {} component file(s) across {} registered module(s)",
            linker_stats.components_flattened,
            linker_stats.modules_registered
        );
    }

    // Step 6.7: Resolve any bare specifiers the flatten pass introduced. When
    // it injects an `import { CdkPortal } from '@angular/cdk/portal'` into a
    // project file, that specifier wasn't known to the initial npm resolution
    // — so we re-scan and resolve the delta, folding the new modules +
    // internal edges into `npm_resolution` so the graph-construction below
    // picks them up.
    //
    // Scope the scan to PROJECT files only. Scanning npm files pulls in
    // spurious specifiers from packages' embedded test/dev code (e.g.
    // `@vitest/*`, `pathe`, `tinyrainbow`) that the app never reaches but
    // which, once linked, can corrupt the evaluation order of unrelated
    // modules — observed as a silent dialog failure in a real-world app.
    let project_modules: HashMap<PathBuf, String> = modules
        .iter()
        .filter(|(path, _)| !ngc_linker::is_npm_path(path))
        .map(|(p, s)| (p.clone(), s.clone()))
        .collect();
    let post_link_specifiers = scan_transformed_bare_specifiers(&project_modules, &local_prefixes);
    let mut new_specifiers: Vec<String> = Vec::new();
    for spec in post_link_specifiers {
        if !bare_specifiers.contains(&spec) {
            new_specifiers.push(spec);
        }
    }
    if !new_specifiers.is_empty() {
        tracing::info!(
            "resolving {} additional specifier(s) introduced by flatten pass: {:?}",
            new_specifiers.len(),
            new_specifiers
        );
        bare_specifiers.extend(new_specifiers.iter().cloned());
        let extra = ngc_npm_resolver::resolve_npm_dependencies(
            &new_specifiers,
            &config_dir,
            export_conditions,
        )?;
        tracing::info!(
            "post-flatten npm resolution pulled in {} file(s)",
            extra.modules.len()
        );
        for (path, source) in &extra.modules {
            modules
                .entry(path.clone())
                .or_insert_with(|| source.clone());
            npm_resolution
                .modules
                .entry(path.clone())
                .or_insert_with(|| source.clone());
        }
        for spec in &new_specifiers {
            npm_resolution.resolved_specifiers.insert(spec.clone());
        }
        for edge in extra.edges {
            npm_resolution.edges.push(edge);
        }
        // Re-run the linker on any newly-pulled-in npm files so their
        // ɵɵngDeclare calls are transformed too.
        let _ = ngc_linker::link_modules(&mut modules, &config_dir)?;
    }
    drop(link_span);

    // Inject vendored helpers for oxc runtime (not an npm dependency of the project)
    let graph_span = tracing::info_span!("graph_assembly").entered();
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
        let resolve_entry = || -> Option<PathBuf> {
            if specifier.starts_with('#') {
                let from_file = import_sites.first().map(|(p, _)| p.as_path());
                ngc_npm_resolver::resolve::resolve_subpath_import(
                    specifier,
                    from_file,
                    &config_dir,
                    export_conditions,
                )
                .ok()
            } else {
                ngc_npm_resolver::resolve::resolve_bare_specifier(
                    specifier,
                    &config_dir,
                    export_conditions,
                )
                .ok()
            }
        };
        if let Some(entry_path) = npm_resolution
            .resolved_specifiers
            .contains(specifier)
            .then(resolve_entry)
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

    // Production-only: substitute Angular's build-time flags with their
    // literal values so the minifier can dead-code-eliminate `if (ngDevMode)`
    // branches throughout `@angular/core` and friends. Replaces the runtime
    // `globalThis.ngDevMode = false` prologue from earlier ngc-rs versions.
    if configuration == Some("production") {
        let define_span = tracing::info_span!("define_substitution").entered();
        ngc_ts_transform::apply_defines_to_modules(
            &mut modules,
            &ngc_ts_transform::DefineMap::production_angular(),
        );
        drop(define_span);
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
        export_conditions: export_conditions.iter().map(|s| (*s).to_string()).collect(),
    };
    drop(graph_span);

    let bundle_span = tracing::info_span!("bundle").entered();
    let bundle_output = ngc_bundler::bundle(&bundle_input)?;
    let modules_bundled: usize = bundle_output
        .chunks
        .values()
        .map(|code| code.matches("\n// ").count() + 1)
        .sum();
    drop(bundle_span);

    // Step 7: Write outputs
    let io_span = tracing::info_span!("io_outputs").entered();
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

    // Step 8: Generate polyfills.js — resolve each entry through the npm
    // resolver (bare specifier) or `ts-transform` (relative .ts), build a
    // synthetic side-effect entry, and run it through the bundler so the
    // output ships resolved code rather than bare ES module specifiers.
    let mut polyfills_filename: Option<String> = None;
    if let Some(ref ap) = angular_project {
        if !ap.polyfills.is_empty() {
            let bundle = polyfills::generate_polyfills(
                &ap.polyfills,
                &out_dir,
                &config_dir,
                bundle_options,
                configuration,
            )?;
            polyfills_filename = Some(bundle.filename);
            output_files.extend(bundle.output_files);
        }
    }

    // Step 9: Finalise the CSS job started back in step 3.5. The styles.css
    // file was already written before bundling; here we just wait for any
    // in-flight PostCSS subprocess to finish overwriting it.
    if let Some(job) = css_job {
        if let Some(child) = job.postcss_child {
            await_postcss(child);
        }
        output_files.push(job.styles_path);
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
            let index_opts = IndexHtmlOptions {
                base_href: ap.base_href.as_deref(),
                deploy_url: ap.deploy_url.as_deref(),
                cross_origin: ap.cross_origin,
                subresource_integrity: ap.subresource_integrity,
            };
            let path = generate_index_html(
                index_path,
                &ap.index_output,
                !ap.styles.is_empty(),
                polyfills_filename.as_deref(),
                &out_dir,
                &bundle_output.main_filename,
                &index_opts,
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

    // Step 12.5: Service worker manifest (`ngsw.json`) when the project opts
    // in via `architect.build.options.serviceWorker`. Hashing runs *after*
    // every other writer so it sees the final filenames + contents.
    if let Some(ref ap) = angular_project {
        if ap.service_worker {
            if localize {
                tracing::warn!(
                    "serviceWorker is enabled but --localize was passed; skipping ngsw.json (per-locale manifests are not yet supported)"
                );
            } else {
                let ngsw_paths = generate_service_worker(ap, &out_dir, &config_dir)?;
                output_files.extend(ngsw_paths);
            }
        }
    }

    // Step 13: --localize → fan the source-locale build out to
    // `<out_dir>/<sourceLocale>/` and produce a translated copy under
    // `<out_dir>/<locale>/` for each entry in `i18n.locales`.
    if localize {
        let i18n = angular_project
            .as_ref()
            .and_then(|ap| ap.i18n.as_ref())
            .ok_or_else(|| NgcError::ConfigError {
                message:
                    "--localize was passed but angular.json does not declare a `projects.<name>.i18n` block"
                        .to_string(),
            })?;
        let localized_files = fan_out_locales(&out_dir, i18n, &output_files)?;
        output_files = localized_files;
    }

    // Compute total size
    let total_size_bytes = output_files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    drop(io_span);

    Ok(BuildResult {
        modules_bundled,
        output_files,
        total_size_bytes,
    })
}

/// Move the source-locale build under `<out_dir>/<sourceLocale>/` and
/// emit a translated copy under `<out_dir>/<locale>/` for every entry in
/// `i18n.locales`. Returns the new full set of output files.
fn fan_out_locales(
    out_dir: &Path,
    i18n: &I18nConfig,
    original_files: &[PathBuf],
) -> NgcResult<Vec<PathBuf>> {
    // Materialize file contents from the original (source-locale) build so
    // we can write them back into per-locale directories without worrying
    // about the source-locale move clobbering them.
    let mut sources: Vec<(PathBuf, Vec<u8>)> = Vec::with_capacity(original_files.len());
    for f in original_files {
        let rel = f.strip_prefix(out_dir).unwrap_or(f).to_path_buf();
        let bytes = std::fs::read(f).map_err(|e| NgcError::Io {
            path: f.clone(),
            source: e,
        })?;
        sources.push((rel, bytes));
    }

    // Remove the original outputs so the source-locale subdirectory takes
    // over the layout cleanly.
    for f in original_files {
        let _ = std::fs::remove_file(f);
    }

    let mut new_outputs: Vec<PathBuf> = Vec::new();

    let source_dir = out_dir.join(&i18n.source_locale);
    write_locale_tree(&source_dir, &sources, None, &mut new_outputs)?;

    for entry in i18n.locales.values() {
        let translations = match &entry.translation_path {
            Some(path) => Some(localize::parse_xliff(path)?),
            None => None,
        };
        let dir = out_dir.join(&entry.locale);
        write_locale_tree(&dir, &sources, translations.as_ref(), &mut new_outputs)?;
    }
    Ok(new_outputs)
}

/// Write `(rel_path, contents)` pairs into `dir`, applying translation
/// substitution to `.js`/`.mjs` files when `translations` is `Some`.
fn write_locale_tree(
    dir: &Path,
    sources: &[(PathBuf, Vec<u8>)],
    translations: Option<&localize::TranslationMap>,
    new_outputs: &mut Vec<PathBuf>,
) -> NgcResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| NgcError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    for (rel, bytes) in sources {
        let target = dir.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NgcError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let final_bytes = match (translations, is_translatable(rel)) {
            (Some(map), true) => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                localize::apply_translations(&text, map).into_bytes()
            }
            _ => bytes.clone(),
        };
        std::fs::write(&target, &final_bytes).map_err(|e| NgcError::Io {
            path: target.clone(),
            source: e,
        })?;
        new_outputs.push(target);
    }
    Ok(())
}

/// Files that may contain `$localize\`...\`` calls and so receive
/// translation substitution. Source maps and binary assets are copied
/// verbatim.
fn is_translatable(rel: &Path) -> bool {
    matches!(
        rel.extension().and_then(|e| e.to_str()),
        Some("js") | Some("mjs")
    )
}

/// Result of a single `extract-i18n` run.
struct ExtractI18nReport {
    message_count: usize,
    out_file: PathBuf,
}

/// Walk every component file reachable from the tsconfig graph, collect
/// translatable messages, and write a deduplicated `messages.xlf` to
/// `out_file` (default: `<project_dir>/messages.xlf`).
fn run_extract_i18n(
    project: &Path,
    out_file: Option<&Path>,
    source_locale_override: Option<&str>,
) -> NgcResult<ExtractI18nReport> {
    let angular_project = find_and_resolve_angular_json(project, None)?;
    let tsconfig_path = angular_project
        .as_ref()
        .map(|ap| ap.ts_config.clone())
        .unwrap_or_else(|| project.to_path_buf());

    let file_graph = ngc_project_resolver::resolve_project(&tsconfig_path)?;
    let files: Vec<PathBuf> = file_graph.graph.node_weights().cloned().collect();

    let mut messages: Vec<(PathBuf, ngc_template_compiler::i18n::ExtractedI18nMessage)> =
        Vec::new();
    for file in &files {
        let extracted = ngc_template_compiler::extract_i18n_from_file(file)?;
        for m in extracted {
            messages.push((file.clone(), m));
        }
    }

    // Deduplicate by id (when present); messages without an id collapse on
    // their source text so two identical occurrences become one trans-unit.
    use std::collections::BTreeMap;
    let mut by_key: BTreeMap<String, (PathBuf, ngc_template_compiler::i18n::ExtractedI18nMessage)> =
        BTreeMap::new();
    for (path, msg) in messages {
        let key = msg
            .id
            .clone()
            .unwrap_or_else(|| auto_id_for(&msg.source, msg.meaning.as_deref()));
        by_key.entry(key).or_insert((path, msg));
    }

    let source_locale = source_locale_override
        .map(String::from)
        .or_else(|| {
            angular_project
                .as_ref()
                .and_then(|ap| ap.i18n.as_ref())
                .map(|i| i.source_locale.clone())
        })
        .unwrap_or_else(|| "en-US".to_string());

    let xlf = build_xliff(&source_locale, &by_key);

    let project_dir = project.parent().unwrap_or(Path::new(".")).to_path_buf();
    let out_path = out_file
        .map(PathBuf::from)
        .unwrap_or_else(|| project_dir.join("messages.xlf"));
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| NgcError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    std::fs::write(&out_path, xlf).map_err(|e| NgcError::Io {
        path: out_path.clone(),
        source: e,
    })?;

    Ok(ExtractI18nReport {
        message_count: by_key.len(),
        out_file: out_path,
    })
}

/// Generate a stable id for messages that do not declare `@@id` explicitly.
/// Hashing the (meaning, source) pair matches Angular's own convention so
/// downstream `xlf-merge` tooling can correlate runs.
fn auto_id_for(source: &str, meaning: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    if let Some(m) = meaning {
        hasher.update(m.as_bytes());
        hasher.update(b"|");
    }
    hasher.update(source.as_bytes());
    let bytes = hasher.finalize();
    bytes.iter().take(10).fold(String::new(), |mut acc, b| {
        acc.push_str(&format!("{b:02x}"));
        acc
    })
}

/// Render a `BTreeMap<id, message>` as an XLIFF 1.2 document.
fn build_xliff(
    source_locale: &str,
    messages: &std::collections::BTreeMap<
        String,
        (PathBuf, ngc_template_compiler::i18n::ExtractedI18nMessage),
    >,
) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n");
    s.push_str(&format!(
        "<xliff version=\"1.2\" xmlns=\"urn:oasis:names:tc:xliff:document:1.2\">\n  <file source-language=\"{}\" datatype=\"plaintext\" original=\"ng2.template\">\n    <body>\n",
        xml_escape(source_locale)
    ));
    for (id, (path, msg)) in messages {
        s.push_str(&format!(
            "      <trans-unit id=\"{}\" datatype=\"html\">\n",
            xml_escape(id)
        ));
        s.push_str(&format!(
            "        <source>{}</source>\n",
            xml_escape(&msg.source)
        ));
        s.push_str(&format!(
            "        <context-group purpose=\"location\">\n          <context context-type=\"sourcefile\">{}</context>\n        </context-group>\n",
            xml_escape(&path.display().to_string())
        ));
        if let Some(m) = &msg.meaning {
            s.push_str(&format!(
                "        <note priority=\"1\" from=\"meaning\">{}</note>\n",
                xml_escape(m)
            ));
        }
        if let Some(d) = &msg.description {
            s.push_str(&format!(
                "        <note priority=\"1\" from=\"description\">{}</note>\n",
                xml_escape(d)
            ));
        }
        s.push_str("      </trans-unit>\n");
    }
    s.push_str("    </body>\n  </file>\n</xliff>\n");
    s
}

/// Minimal XML attribute / text escaping for the five canonical entities.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
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
            tree_shake: true,
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

/// Read and concatenate global style files, writing dist/styles.css.
///
/// Accepts `.css`, `.scss`, `.sass`, `.less`, and `.styl`/`.stylus` files.
/// Non-CSS entries are preprocessed through the appropriate Node subprocess
/// (`sass` / `less` / `stylus`) before concatenation. After concatenation,
/// CSS `@import` directives that reference npm packages (e.g.
/// `@import "tailwindcss"`) are resolved by looking up the package in
/// `node_modules`.
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
        let language = StyleLanguage::from_extension(ext);
        let content = if language == StyleLanguage::Css {
            std::fs::read_to_string(&style.path).map_err(|e| NgcError::Io {
                path: style.path.clone(),
                source: e,
            })?
        } else {
            ngc_template_compiler::preprocessor::preprocess_file(&style.path, project_root)?
        };
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

        // Preserve @config directives — PostCSS/Tailwind needs them
        if trimmed.starts_with("@config") {
            result.push_str(line);
            result.push('\n');
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

/// Options controlling how `index.html` is rewritten during injection.
///
/// Each field mirrors the corresponding `angular.json` build option. See
/// [`generate_index_html`] for how they're applied to the emitted HTML.
#[derive(Debug, Default, Clone, Copy)]
struct IndexHtmlOptions<'a> {
    /// `baseHref` — value written into `<base href="...">`.
    base_href: Option<&'a str>,
    /// `deployUrl` — absolute URL prefix prepended to injected `src`/`href`.
    deploy_url: Option<&'a str>,
    /// `crossOrigin` attribute for injected `<script>` / `<link>` tags.
    cross_origin: CrossOrigin,
    /// When true, compute SHA-384 integrity hashes of emitted artifacts and
    /// inject `integrity="sha384-..."` attributes on the tags that load them.
    subresource_integrity: bool,
}

/// Read source index.html, inject stylesheet and script tags, write to out_dir.
///
/// Applies the `baseHref`, `deployUrl`, `crossOrigin`, and
/// `subresourceIntegrity` options during injection. SRI hashes are computed
/// over the files on disk in `out_dir`, so this must run after the bundler,
/// polyfills, and CSS pipeline have emitted their artifacts.
fn generate_index_html(
    index_source: &Path,
    output_filename: &str,
    has_styles: bool,
    polyfills_filename: Option<&str>,
    out_dir: &Path,
    main_filename: &str,
    options: &IndexHtmlOptions,
) -> NgcResult<PathBuf> {
    let mut html = std::fs::read_to_string(index_source).map_err(|e| NgcError::Io {
        path: index_source.to_path_buf(),
        source: e,
    })?;

    // Rewrite or inject <base href="..."> per baseHref option.
    if let Some(base_href) = options.base_href {
        html = apply_base_href(&html, base_href);
    }

    let deploy_url = options.deploy_url.unwrap_or("");
    let crossorigin_attr = options
        .cross_origin
        .attribute_value()
        .map(|v| format!(" crossorigin=\"{v}\""))
        .unwrap_or_default();

    // Inject stylesheet link before </head>
    if has_styles {
        let href = format!("{deploy_url}styles.css");
        let integrity = if options.subresource_integrity {
            Some(compute_sri_hash(&out_dir.join("styles.css"))?)
        } else {
            None
        };
        let integrity_attr = integrity
            .as_deref()
            .map(|i| format!(" integrity=\"{i}\""))
            .unwrap_or_default();
        let tag = format!(
            "  <link rel=\"stylesheet\" href=\"{href}\"{crossorigin_attr}{integrity_attr}>\n"
        );
        html = html.replace("</head>", &format!("{tag}</head>"));
    }

    // Inject script tags before </body>
    let mut scripts = String::new();
    if let Some(polyfills) = polyfills_filename {
        let src = format!("{deploy_url}{polyfills}");
        let integrity = if options.subresource_integrity {
            Some(compute_sri_hash(&out_dir.join(polyfills))?)
        } else {
            None
        };
        let integrity_attr = integrity
            .as_deref()
            .map(|i| format!(" integrity=\"{i}\""))
            .unwrap_or_default();
        scripts.push_str(&format!(
            "  <script src=\"{src}\" type=\"module\"{crossorigin_attr}{integrity_attr}></script>\n"
        ));
    }
    {
        let src = format!("{deploy_url}{main_filename}");
        let integrity = if options.subresource_integrity {
            Some(compute_sri_hash(&out_dir.join(main_filename))?)
        } else {
            None
        };
        let integrity_attr = integrity
            .as_deref()
            .map(|i| format!(" integrity=\"{i}\""))
            .unwrap_or_default();
        scripts.push_str(&format!(
            "  <script src=\"{src}\" type=\"module\"{crossorigin_attr}{integrity_attr}></script>\n"
        ));
    }
    html = html.replace("</body>", &format!("{scripts}</body>"));

    let path = out_dir.join(output_filename);
    std::fs::write(&path, &html).map_err(|e| NgcError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// Rewrite an existing `<base href="...">` tag, or inject one after `<head>`.
///
/// Matches any existing tag with flexible whitespace and quote styles so we
/// don't create a duplicate when the source index already declares one.
fn apply_base_href(html: &str, base_href: &str) -> String {
    let base_re = regex::Regex::new(r#"<base\s+[^>]*href\s*=\s*["'][^"']*["'][^>]*>"#)
        .expect("valid base-href regex");
    let replacement = format!("<base href=\"{base_href}\">");
    if base_re.is_match(html) {
        return base_re.replace(html, replacement.as_str()).into_owned();
    }

    // No existing <base> tag — inject after the opening <head>.
    let head_re = regex::Regex::new(r"(?i)<head(\s[^>]*)?>").expect("valid head regex");
    if let Some(m) = head_re.find(html) {
        let end = m.end();
        let mut out = String::with_capacity(html.len() + replacement.len() + 4);
        out.push_str(&html[..end]);
        out.push_str("\n  ");
        out.push_str(&replacement);
        out.push_str(&html[end..]);
        return out;
    }
    // No <head> at all — fall back to prepending so the tag still lands.
    format!("{replacement}\n{html}")
}

/// Compute a `sha384-<base64>` SRI digest for a file on disk.
fn compute_sri_hash(path: &Path) -> NgcResult<String> {
    use base64::Engine;
    use sha2::{Digest, Sha384};

    let bytes = std::fs::read(path).map_err(|e| NgcError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let digest = Sha384::digest(&bytes);
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    Ok(format!("sha384-{b64}"))
}

/// Scan the bundle's external imports, find LICENSE files in node_modules,
/// and concatenate them into 3rdpartylicenses.txt.
/// Build the `ngsw.json` manifest and copy the worker scripts into
/// `out_dir`. Reads `architect.build.options.ngswConfigPath` (default
/// `ngsw-config.json`), hashes every emitted file, and writes the manifest
/// alongside the bundle. Returns the list of files written so they can be
/// reported in the build summary.
fn generate_service_worker(
    project: &ResolvedAngularProject,
    out_dir: &Path,
    project_root: &Path,
) -> NgcResult<Vec<PathBuf>> {
    let span = tracing::info_span!("ngsw").entered();
    let config = ngsw::load_config(&project.ngsw_config_path)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut paths = Vec::new();
    paths.push(ngsw::generate_manifest(out_dir, &config, timestamp)?);
    paths.extend(ngsw::copy_worker_scripts(out_dir, project_root)?);
    drop(span);
    Ok(paths)
}

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

/// A CSS job that has been started early in the pipeline so its Node/PostCSS
/// subprocess can run concurrently with the bundler.
struct CssJob {
    /// Path of the concatenated (pre-PostCSS) styles.css on disk.
    styles_path: PathBuf,
    /// Handle to the PostCSS Node subprocess, if `@tailwindcss/postcss` is
    /// installed. `None` means the styles.css file is already final.
    postcss_child: Option<std::process::Child>,
}

/// Extract global styles to `out_dir/styles.css` and, if Tailwind's PostCSS
/// plugin is installed, spawn the Node subprocess to process it.
///
/// Called early in the build so the subprocess overlaps with the bundler
/// (Node startup + Tailwind compile typically costs ~200 ms on a real-world
/// app). The returned [`CssJob`] is awaited later during the output phase.
fn start_css_job(
    angular_project: Option<&ResolvedAngularProject>,
    out_dir: &Path,
    config_dir: &Path,
) -> NgcResult<Option<CssJob>> {
    let Some(ap) = angular_project else {
        return Ok(None);
    };
    if ap.styles.is_empty() {
        return Ok(None);
    }

    std::fs::create_dir_all(out_dir).map_err(|e| NgcError::Io {
        path: out_dir.to_path_buf(),
        source: e,
    })?;

    let styles_path = extract_global_styles(&ap.styles, out_dir, config_dir)?;
    let postcss_child = spawn_postcss(&styles_path, config_dir);
    Ok(Some(CssJob {
        styles_path,
        postcss_child,
    }))
}

/// Spawn the Tailwind-via-PostCSS Node subprocess, returning a handle.
///
/// Returns `None` when `@tailwindcss/postcss` is not installed in the project.
fn spawn_postcss(css_path: &Path, project_dir: &Path) -> Option<std::process::Child> {
    let postcss_pkg = project_dir.join("node_modules/@tailwindcss/postcss");
    if !postcss_pkg.is_dir() {
        return None;
    }

    let script = format!(
        r#"
const postcss = require('postcss');
const tailwindcss = require('@tailwindcss/postcss');
const fs = require('fs');
const css = fs.readFileSync('{}', 'utf8');
postcss([tailwindcss]).process(css, {{ from: 'src/styles.css' }}).then(result => {{
    fs.writeFileSync('{}', result.css);
}}).catch(err => {{
    console.error('PostCSS error:', err.message);
    process.exit(1);
}});
"#,
        css_path.display(),
        css_path.display()
    );

    std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .current_dir(project_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!(
                "{} could not run node for PostCSS: {}",
                "Warning:".yellow().bold(),
                e
            );
        })
        .ok()
}

/// Block until the PostCSS Node subprocess exits and log its outcome.
fn await_postcss(child: std::process::Child) {
    match child.wait_with_output() {
        Ok(output) => {
            if output.status.success() {
                tracing::info!("compiled CSS with PostCSS + Tailwind");
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "{} PostCSS compilation failed: {}",
                    "Warning:".yellow().bold(),
                    stderr.trim()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "{} PostCSS subprocess failed: {}",
                "Warning:".yellow().bold(),
                e
            );
        }
    }
}

/// Build the [`StyleContext`] used by the template compiler for component
/// style preprocessing.
///
/// `project_root` points at the directory containing `node_modules`, which
/// defaults to the tsconfig's directory (`config_dir`) but is overridden by
/// the `angular.json` project root when present — that is where
/// `@angular/build:application` looks for style preprocessor packages.
fn build_style_context(
    angular_project: Option<&ResolvedAngularProject>,
    config_dir: &Path,
) -> StyleContext {
    let project_root = angular_project
        .map(|ap| ap.root.clone())
        .unwrap_or_else(|| config_dir.to_path_buf());
    let inline_style_language = angular_project
        .map(|ap| inline_language_to_style_language(ap.inline_style_language))
        .unwrap_or(StyleLanguage::Css);
    StyleContext {
        project_root,
        inline_style_language,
    }
}

/// Bridge `ngc_project_resolver`'s `InlineStyleLanguage` to the
/// `ngc_template_compiler`'s `StyleLanguage`. The two enums are intentionally
/// separate so template-compiler doesn't depend on project-resolver.
fn inline_language_to_style_language(lang: InlineStyleLanguage) -> StyleLanguage {
    match lang {
        InlineStyleLanguage::Css => StyleLanguage::Css,
        InlineStyleLanguage::Scss => StyleLanguage::Scss,
        InlineStyleLanguage::Sass => StyleLanguage::Sass,
        InlineStyleLanguage::Less => StyleLanguage::Less,
        InlineStyleLanguage::Stylus => StyleLanguage::Stylus,
    }
}

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
                    // The compiled source (with AOT ɵcmp metadata) failed to
                    // transform. Emit it as-is — the bundler uses SourceType::tsx()
                    // and can handle remaining TS annotations. Preserving the AOT
                    // metadata is critical to avoid JIT compilation at runtime.
                    Ok(ngc_ts_transform::TransformedModule {
                        source_path: path.clone(),
                        code: source.clone(),
                        source_map: None,
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
    fn test_index_html_injection() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let index_src = dir.path().join("index.html");
        std::fs::write(
            &index_src,
            "<!doctype html>\n<html>\n<head>\n</head>\n<body>\n  <app-root></app-root>\n</body>\n</html>\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("styles.css"), "").unwrap();
        std::fs::write(dir.path().join("polyfills.js"), "").unwrap();
        std::fs::write(dir.path().join("main.js"), "").unwrap();
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &IndexHtmlOptions::default(),
        )
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
        std::fs::write(dir.path().join("main.js"), "").unwrap();
        let out = generate_index_html(
            &index_src,
            "index.html",
            false,
            None,
            dir.path(),
            "main.js",
            &IndexHtmlOptions::default(),
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(!content.contains("styles.css"));
        assert!(!content.contains("polyfills.js"));
        assert!(content.contains(r#"<script src="main.js" type="module"></script>"#));
    }

    /// Fixture helper: writes a minimal index + artifact files and returns
    /// `(dir, index_src)` for reuse across option-specific tests.
    fn setup_index_fixture(artifact_bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let index_src = dir.path().join("index.html");
        std::fs::write(
            &index_src,
            "<!doctype html>\n<html>\n<head>\n</head>\n<body>\n  <app-root></app-root>\n</body>\n</html>\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("styles.css"), artifact_bytes).unwrap();
        std::fs::write(dir.path().join("polyfills.js"), artifact_bytes).unwrap();
        std::fs::write(dir.path().join("main.js"), artifact_bytes).unwrap();
        (dir, index_src)
    }

    #[test]
    fn test_index_html_base_href_injection() {
        let (dir, index_src) = setup_index_fixture(b"");
        let opts = IndexHtmlOptions {
            base_href: Some("/app/"),
            ..IndexHtmlOptions::default()
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(content.contains(r#"<base href="/app/">"#));
    }

    #[test]
    fn test_index_html_base_href_rewrites_existing() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let index_src = dir.path().join("index.html");
        std::fs::write(
            &index_src,
            "<!doctype html>\n<html>\n<head><base href=\"/\"></head>\n<body></body>\n</html>\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("main.js"), b"").unwrap();
        let opts = IndexHtmlOptions {
            base_href: Some("/app/"),
            ..IndexHtmlOptions::default()
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            false,
            None,
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(content.contains(r#"<base href="/app/">"#));
        // Original href=/ should be gone — only one <base> tag remains.
        assert_eq!(content.matches("<base ").count(), 1);
    }

    #[test]
    fn test_index_html_deploy_url_prefixing() {
        let (dir, index_src) = setup_index_fixture(b"");
        let opts = IndexHtmlOptions {
            deploy_url: Some("https://cdn.example.com/"),
            ..IndexHtmlOptions::default()
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(content.contains(r#"href="https://cdn.example.com/styles.css""#));
        assert!(content.contains(r#"src="https://cdn.example.com/polyfills.js""#));
        assert!(content.contains(r#"src="https://cdn.example.com/main.js""#));
    }

    #[test]
    fn test_index_html_cross_origin_anonymous() {
        let (dir, index_src) = setup_index_fixture(b"");
        let opts = IndexHtmlOptions {
            cross_origin: CrossOrigin::Anonymous,
            ..IndexHtmlOptions::default()
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert_eq!(content.matches(r#"crossorigin="anonymous""#).count(), 3);
    }

    #[test]
    fn test_index_html_cross_origin_use_credentials() {
        let (dir, index_src) = setup_index_fixture(b"");
        let opts = IndexHtmlOptions {
            cross_origin: CrossOrigin::UseCredentials,
            ..IndexHtmlOptions::default()
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(content.contains(r#"crossorigin="use-credentials""#));
    }

    #[test]
    fn test_index_html_subresource_integrity() {
        use base64::Engine;
        use sha2::{Digest, Sha384};
        let payload = b"hello";
        let (dir, index_src) = setup_index_fixture(payload);
        let digest = Sha384::digest(payload);
        let expected = format!(
            "sha384-{}",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );
        let opts = IndexHtmlOptions {
            subresource_integrity: true,
            ..IndexHtmlOptions::default()
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert_eq!(content.matches(expected.as_str()).count(), 3);
    }

    #[test]
    fn test_index_html_all_deploy_options_together() {
        let (dir, index_src) = setup_index_fixture(b"payload");
        let opts = IndexHtmlOptions {
            base_href: Some("/app/"),
            deploy_url: Some("https://cdn.example.com/"),
            cross_origin: CrossOrigin::Anonymous,
            subresource_integrity: true,
        };
        let out = generate_index_html(
            &index_src,
            "index.html",
            true,
            Some("polyfills.js"),
            dir.path(),
            "main.js",
            &opts,
        )
        .unwrap();
        let content = std::fs::read_to_string(out).unwrap();
        assert!(content.contains(r#"<base href="/app/">"#));
        assert!(content.contains(r#"href="https://cdn.example.com/styles.css""#));
        assert!(content.contains(r#"src="https://cdn.example.com/polyfills.js""#));
        assert!(content.contains(r#"src="https://cdn.example.com/main.js""#));
        assert_eq!(content.matches(r#"crossorigin="anonymous""#).count(), 3);

        // Verify SRI hashes match what openssl would emit for the payload.
        use base64::Engine;
        use sha2::{Digest, Sha384};
        let digest = Sha384::digest(b"payload");
        let expected = format!(
            "sha384-{}",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );
        assert_eq!(content.matches(expected.as_str()).count(), 3);
    }

    #[test]
    fn test_build_options_production() {
        let opts = build_options(Some("production"));
        assert!(opts.source_maps);
        assert!(opts.minify);
        assert!(opts.content_hash);
        assert!(opts.tree_shake);
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

    /// Scaffold a `public/` tree mirroring a typical Angular 17+ app layout.
    fn scaffold_public_tree(root: &Path) -> PathBuf {
        let public = root.join("public");
        std::fs::create_dir_all(public.join("i18n")).unwrap();
        std::fs::create_dir_all(public.join("nested/deep")).unwrap();
        std::fs::write(public.join("i18n/de.json"), r#"{"hello":"hallo"}"#).unwrap();
        std::fs::write(public.join("i18n/en.json"), r#"{"hello":"hello"}"#).unwrap();
        std::fs::write(public.join("appleid_button@4x.png"), b"\x89PNG\r\n").unwrap();
        std::fs::write(public.join("nested/deep/file.txt"), "leaf").unwrap();
        public
    }

    /// Collect files under `dir`, returning paths relative to `dir` with
    /// forward-slash separators — deterministic output for snapshots.
    fn collect_relative_files(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    let rel = path.strip_prefix(base).unwrap();
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut files = Vec::new();
        walk(dir, dir, &mut files);
        files.sort();
        files
    }

    #[test]
    fn test_copy_assets_public_folder_glob() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let public = scaffold_public_tree(dir.path());
        let out = dir.path().join("dist");
        std::fs::create_dir_all(&out).unwrap();

        let asset = ResolvedAsset::Glob {
            pattern: "**/*".to_string(),
            input: public,
            output: "/".to_string(),
            ignore: vec![],
        };
        copy_assets(&[asset], &out).expect("copy_assets should succeed");

        let files = collect_relative_files(&out);
        insta::assert_snapshot!("public_folder_glob_dist", files.join("\n"));
    }

    #[test]
    fn test_copy_assets_glob_with_output_subdir() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let public = scaffold_public_tree(dir.path());
        let out = dir.path().join("dist");
        std::fs::create_dir_all(&out).unwrap();

        let asset = ResolvedAsset::Glob {
            pattern: "**/*".to_string(),
            input: public,
            output: "/assets/".to_string(),
            ignore: vec![],
        };
        copy_assets(&[asset], &out).expect("copy_assets should succeed");

        let files = collect_relative_files(&out);
        insta::assert_snapshot!("public_folder_glob_output_subdir", files.join("\n"));
    }

    /// End-to-end fixture: angular.json with `serviceWorker: true` plus a
    /// minimal `ngsw-config.json` and a fake dist tree must produce a valid
    /// `dist/ngsw.json` with hashes matching the actual served bytes.
    #[test]
    fn test_service_worker_pipeline_pwa_fixture() {
        use ngc_project_resolver::angular_json::resolve_angular_project;
        use sha1::{Digest, Sha1};

        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // Minimal angular.json that opts into the service-worker pipeline.
        std::fs::write(
            root.join("angular.json"),
            r#"{
                "projects": {
                    "pwa": {
                        "root": "",
                        "sourceRoot": "src",
                        "architect": {
                            "build": {
                                "options": {
                                    "outputPath": "dist/pwa",
                                    "tsConfig": "tsconfig.json",
                                    "serviceWorker": true,
                                    "ngswConfigPath": "ngsw-config.json"
                                }
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        // Minimal ngsw-config.json: an "app" prefetch group pulling the
        // shell + JS, plus a "media" lazy group for assets.
        std::fs::write(
            root.join("ngsw-config.json"),
            r#"{
                "$schema": "./node_modules/@angular/service-worker/config/schema.json",
                "index": "/index.html",
                "assetGroups": [
                    {
                        "name": "app",
                        "installMode": "prefetch",
                        "resources": {
                            "files": ["/index.html", "/*.js", "/*.css"]
                        }
                    },
                    {
                        "name": "media",
                        "installMode": "lazy",
                        "updateMode": "prefetch",
                        "resources": {
                            "files": ["/assets/**"]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        // Pre-populate the dist tree as if the bundler had already run.
        let dist = root.join("dist").join("pwa");
        std::fs::create_dir_all(dist.join("assets")).unwrap();
        let index_bytes = b"<!doctype html><title>pwa</title>";
        let main_bytes = b"console.log('main')";
        let style_bytes = b"body { color: red; }";
        let logo_bytes = b"<svg/>";
        std::fs::write(dist.join("index.html"), index_bytes).unwrap();
        std::fs::write(dist.join("main-ABCDE.js"), main_bytes).unwrap();
        std::fs::write(dist.join("styles-FGHIJ.css"), style_bytes).unwrap();
        std::fs::write(dist.join("assets").join("logo.svg"), logo_bytes).unwrap();

        // Resolve angular.json: confirms the parser picked up serviceWorker.
        let project = resolve_angular_project(&root.join("angular.json"), Some("pwa"), None)
            .expect("resolve angular project");
        assert!(project.service_worker);

        // Run the full SW pipeline (load config + walk dist + emit manifest).
        let ngsw_paths =
            generate_service_worker(&project, &dist, root).expect("generate_service_worker");
        assert!(!ngsw_paths.is_empty(), "should write at least ngsw.json");

        let manifest_path = dist.join("ngsw.json");
        assert!(manifest_path.is_file(), "ngsw.json must exist");
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap())
                .expect("ngsw.json parses as JSON");

        // Required Angular SW protocol fields.
        assert_eq!(manifest["configVersion"], 1);
        assert_eq!(manifest["index"], "/index.html");
        assert!(manifest["timestamp"].as_u64().is_some());
        assert_eq!(manifest["navigationRequestStrategy"], "performance");
        assert!(manifest["navigationUrls"].is_array());
        assert!(!manifest["navigationUrls"].as_array().unwrap().is_empty());

        // assetGroups is shaped as Angular expects.
        let groups = manifest["assetGroups"].as_array().expect("assetGroups");
        assert_eq!(groups.len(), 2);
        let app = &groups[0];
        assert_eq!(app["name"], "app");
        assert_eq!(app["installMode"], "prefetch");
        assert_eq!(app["updateMode"], "prefetch");
        let app_urls: Vec<&str> = app["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(app_urls.contains(&"/index.html"));
        assert!(app_urls.contains(&"/main-ABCDE.js"));
        assert!(app_urls.contains(&"/styles-FGHIJ.css"));

        let media = &groups[1];
        assert_eq!(media["name"], "media");
        assert_eq!(media["installMode"], "lazy");
        assert_eq!(media["updateMode"], "prefetch");
        let media_urls: Vec<&str> = media["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(media_urls, vec!["/assets/logo.svg"]);

        // hashTable hashes must equal the SHA-1 of the actual served bytes.
        let table = manifest["hashTable"].as_object().expect("hashTable");
        let expect_sha1 = |bytes: &[u8]| -> String {
            let mut h = Sha1::new();
            h.update(bytes);
            let digest = h.finalize();
            digest.iter().fold(String::new(), |mut acc, b| {
                acc.push_str(&format!("{b:02x}"));
                acc
            })
        };
        assert_eq!(
            table["/index.html"].as_str().unwrap(),
            expect_sha1(index_bytes)
        );
        assert_eq!(
            table["/main-ABCDE.js"].as_str().unwrap(),
            expect_sha1(main_bytes)
        );
        assert_eq!(
            table["/styles-FGHIJ.css"].as_str().unwrap(),
            expect_sha1(style_bytes)
        );
        assert_eq!(
            table["/assets/logo.svg"].as_str().unwrap(),
            expect_sha1(logo_bytes)
        );
        // Every URL in any group must have a hashTable entry.
        for url in app_urls.iter().chain(media_urls.iter()) {
            assert!(table.contains_key(*url), "missing hash for {url}");
        }
    }
}
