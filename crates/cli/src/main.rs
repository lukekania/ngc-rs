use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};
use colored::Colorize;
use ngc_diagnostics::{NgcError, NgcResult};
use ngc_ts_transform::TransformResult;

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
    /// Build the project: transform TypeScript files to JavaScript.
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
                println!("  {:<16}{}", "Files:".dimmed(), result.files_transformed);
                println!("  {:<16}{}", "Output:".dimmed(), result.out_dir.display());
            }
            Err(e) => {
                eprintln!("{} {e}", "Error:".red().bold());
                process::exit(1);
            }
        },
    }
}

/// Orchestrate the full build pipeline: resolve project, then transform all files.
fn run_build(project: &Path, out_dir_override: Option<&Path>) -> NgcResult<TransformResult> {
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
        .unwrap_or_else(|| config_dir.join("out"));

    let files: Vec<PathBuf> = file_graph.graph.node_weights().cloned().collect();

    ngc_ts_transform::transform_project(&files, &root_dir, &out_dir)
}
