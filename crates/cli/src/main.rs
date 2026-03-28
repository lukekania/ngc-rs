use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};
use colored::Colorize;
use ngc_bundler::BundleInput;
use ngc_diagnostics::{NgcError, NgcResult};

/// Result of the bundled build pipeline.
struct BuildResult {
    /// Number of modules included in the bundle.
    modules_bundled: usize,
    /// Path to the output bundle file.
    output_path: PathBuf,
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
        /// Output directory (overrides tsconfig outDir).
        #[arg(long)]
        out_dir: Option<PathBuf>,
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
        Commands::Build { project, out_dir } => match run_build(&project, out_dir.as_deref()) {
            Ok(result) => {
                println!("{}", "ngc-rs build complete".bold().green());
                println!("  {:<16}{}", "Bundled:".dimmed(), result.modules_bundled);
                println!(
                    "  {:<16}{}",
                    "Output:".dimmed(),
                    result.output_path.display()
                );
            }
            Err(e) => {
                eprintln!("{} {e}", "Error:".red().bold());
                process::exit(1);
            }
        },
    }
}

/// Orchestrate the full build pipeline: resolve → transform in memory → bundle → write.
fn run_build(project: &Path, out_dir_override: Option<&Path>) -> NgcResult<BuildResult> {
    let config = ngc_project_resolver::tsconfig::resolve_tsconfig(project)?;
    let file_graph = ngc_project_resolver::resolve_project(project)?;

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

    let out_dir = out_dir_override
        .map(PathBuf::from)
        .or_else(|| {
            config
                .compiler_options
                .out_dir
                .as_ref()
                .map(|o| config_dir.join(o))
        })
        .unwrap_or_else(|| config_dir.join("dist"));

    // Transform all files to memory
    let files: Vec<PathBuf> = file_graph.graph.node_weights().cloned().collect();
    let transformed = ngc_ts_transform::transform_to_memory(&files)?;

    // Build modules map (canonical source path → JS code)
    let modules: HashMap<PathBuf, String> = transformed
        .into_iter()
        .map(|m| (m.source_path, m.code))
        .collect();

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

    let bundle_input = BundleInput {
        modules,
        graph: file_graph.graph,
        entry,
        local_prefixes,
        root_dir,
    };

    let bundle_output = ngc_bundler::bundle(&bundle_input)?;
    let modules_bundled = bundle_output.matches("\n// ").count() + 1;

    // Write the bundle
    std::fs::create_dir_all(&out_dir).map_err(|e| NgcError::Io {
        path: out_dir.clone(),
        source: e,
    })?;
    let output_path = out_dir.join("main.js");
    std::fs::write(&output_path, &bundle_output).map_err(|e| NgcError::Io {
        path: output_path.clone(),
        source: e,
    })?;

    Ok(BuildResult {
        modules_bundled,
        output_path,
    })
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
