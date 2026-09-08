mod apply;
mod storage;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "wg-server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate/reuse keys, render configs, and publish a new generation
    /// if anything changed.
    Apply {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Validate the topology config only; no key handling, no network.
    Validate {
        #[arg(long)]
        config: PathBuf,
    },
}

fn read_config(path: &PathBuf) -> anyhow::Result<wg_common::topology::TopologyConfig> {
    let text = std::fs::read_to_string(path)?;
    wg_common::topology::parse(&text).map_err(|e| anyhow::anyhow!("{e}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Validate { config } => match read_config(&config) {
            Ok(cfg) => match wg_common::topology::validate(&cfg) {
                Ok((topo, warnings)) => {
                    for w in &warnings {
                        tracing::warn!("{w}");
                    }
                    println!(
                        "OK: {} nodes, {} edges",
                        topo.nodes.len(),
                        topo.edges().len()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("validation error: {e}");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("config error: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Apply { config, dry_run } => {
            let cfg = match read_config(&config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("config error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let client = storage::R2Client::new(&cfg.r2_config);
            let opts = apply::ApplyOptions { dry_run };
            match apply::apply(&client, &cfg, &opts).await {
                Ok(report) => {
                    for w in &report.warnings {
                        tracing::warn!("{w}");
                    }
                    println!(
                        "reused={} freshly_keyed={} published={} generation={:?} uploaded={}",
                        report.reused_hostnames.len(),
                        report.freshly_keyed_hostnames.len(),
                        report.published,
                        report.new_generation_id,
                        report.uploaded_hostnames.len(),
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("apply failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
