mod apply;
mod retention;
mod storage;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "wg-server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Increase log verbosity (-v = info, -vv = debug, -vvv = trace).
    /// Ignored if RUST_LOG is set.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
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
        /// Force a fresh key for the named node(s) (repeatable), or for
        /// every node if given with no value. Cannot mix both forms.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        rotate: Vec<String>,
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

/// `--rotate` bare (empty string, via `default_missing_value`) means
/// "rotate every node"; `--rotate <hostname>` (repeatable) names specific
/// ones. Mixing both forms in one invocation is a validation error.
fn parse_rotate(raw: Vec<String>) -> anyhow::Result<apply::RotateSelection> {
    if raw.is_empty() {
        return Ok(apply::RotateSelection::None);
    }
    let bare_count = raw.iter().filter(|s| s.is_empty()).count();
    if bare_count > 0 {
        if bare_count < raw.len() {
            anyhow::bail!(
                "cannot combine a bare --rotate (rotate every node) with --rotate <hostname>"
            );
        }
        return Ok(apply::RotateSelection::All);
    }
    Ok(apply::RotateSelection::Named(raw.into_iter().collect()))
}

/// RUST_LOG, if set, always wins; otherwise `-v`/`-vv`/`-vvv` selects
/// info/debug/trace, defaulting to warn with no flag at all.
fn build_env_filter(verbose: u8) -> tracing_subscriber::EnvFilter {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        return filter;
    }
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::EnvFilter::new(level)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(build_env_filter(cli.verbose))
        .init();

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
        Command::Apply {
            config,
            dry_run,
            rotate,
        } => {
            let cfg = match read_config(&config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("config error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let rotate = match parse_rotate(rotate) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            let client = storage::R2Client::new(&cfg.r2_config);
            let opts = apply::ApplyOptions {
                dry_run,
                rotate,
                ..Default::default()
            };
            match apply::apply(&client, &cfg, &opts).await {
                Ok(report) => {
                    for w in &report.warnings {
                        tracing::warn!("{w}");
                    }
                    println!(
                        "reused={} freshly_keyed={} published={} generation={:?} uploaded={} cleanup_deleted={} cleanup_skipped_within_grace={}",
                        report.reused_hostnames.len(),
                        report.freshly_keyed_hostnames.len(),
                        report.published,
                        report.new_generation_id,
                        report.uploaded_hostnames.len(),
                        report.cleanup_deleted.len(),
                        report.cleanup_skipped_within_grace.len(),
                    );
                    if let Some(err) = &report.cleanup_error {
                        eprintln!("cleanup incomplete: {err}");
                        return ExitCode::FAILURE;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rotate_arg(cli_args: &[&str]) -> Vec<String> {
        let mut args = vec!["wg-server", "apply", "--config", "x.json"];
        args.extend_from_slice(cli_args);
        match Cli::try_parse_from(args).unwrap().command {
            Command::Apply { rotate, .. } => rotate,
            _ => unreachable!(),
        }
    }

    #[test]
    fn no_rotate_flag_means_none() {
        assert!(matches!(
            parse_rotate(rotate_arg(&[])).unwrap(),
            apply::RotateSelection::None
        ));
    }

    #[test]
    fn bare_rotate_means_all() {
        assert!(matches!(
            parse_rotate(rotate_arg(&["--rotate"])).unwrap(),
            apply::RotateSelection::All
        ));
    }

    #[test]
    fn repeated_named_rotate_collects_hostnames() {
        let selection = parse_rotate(rotate_arg(&["--rotate", "a", "--rotate", "b"])).unwrap();
        match selection {
            apply::RotateSelection::Named(set) => {
                assert_eq!(
                    set,
                    ["a".to_string(), "b".to_string()].into_iter().collect()
                );
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn mixing_bare_and_named_rotate_is_rejected() {
        assert!(parse_rotate(rotate_arg(&["--rotate", "--rotate", "a"])).is_err());
    }
}
