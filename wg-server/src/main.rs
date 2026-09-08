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

/// The topology config path can come from `--config`, or from this
/// environment variable — so the binary can run unmodified as a cloud
/// cron job with the path injected via environment. `--config` wins if
/// both are set.
const CONFIG_ENV_VAR: &str = "WG_SERVER_CONFIG";

#[derive(Subcommand)]
enum Command {
    /// Generate/reuse keys, render configs, and publish a new generation
    /// if anything changed.
    Apply {
        /// Path to the topology JSON. Falls back to WG_SERVER_CONFIG if
        /// not given.
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        /// Force a fresh key for the named node(s) (repeatable), or for
        /// every node if given with no value. Cannot mix both forms.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        rotate: Vec<String>,
        /// Force immediate cleanup of any generation that is neither
        /// current nor previous, bypassing the default 15-minute grace
        /// period. Cleanup always runs on every apply; this only removes
        /// the delay, so use it when you know no concurrent apply is in
        /// flight (e.g. interactive/manual use) rather than routinely.
        #[arg(long)]
        prune: bool,
    },
    /// Validate the topology config only; no key handling, no network.
    Validate {
        /// Path to the topology JSON. Falls back to WG_SERVER_CONFIG if
        /// not given.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Resolves `--config`, falling back to `WG_SERVER_CONFIG` so the same
/// binary can run as a cloud cron job with the path injected via
/// environment rather than a fixed CLI argument (docs/wg-server.md §1).
fn resolve_config_path(cli_value: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = cli_value {
        return Ok(path);
    }
    std::env::var_os(CONFIG_ENV_VAR)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--config not given and {CONFIG_ENV_VAR} is not set"))
}

fn read_config(path: &PathBuf) -> anyhow::Result<wg_common::topology::TopologyConfig> {
    tracing::debug!(path = %path.display(), "reading topology config");
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
        Command::Validate { config } => {
            let config = match resolve_config_path(config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            match read_config(&config) {
                Ok(cfg) => match wg_common::topology::validate(&cfg) {
                    Ok((topo, warnings)) => {
                        tracing::info!(
                            nodes = topo.nodes.len(),
                            edges = topo.edges().len(),
                            "validated"
                        );
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
            }
        }
        Command::Apply {
            config,
            dry_run,
            rotate,
            prune,
        } => {
            let config = match resolve_config_path(config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
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
            tracing::info!(
                nodes = cfg.nodes.len(),
                masters = cfg.master.len(),
                dry_run,
                prune,
                rotate = ?rotate,
                "starting apply"
            );
            let client = storage::R2Client::new(&cfg.r2_config);
            let opts = apply::ApplyOptions {
                dry_run,
                rotate,
                grace_period: if prune {
                    std::time::Duration::ZERO
                } else {
                    apply::ApplyOptions::default().grace_period
                },
            };
            match apply::apply(&client, &cfg, &opts).await {
                Ok(report) => {
                    for w in &report.warnings {
                        tracing::warn!("{w}");
                    }
                    tracing::info!(
                        reused = report.reused_hostnames.len(),
                        freshly_keyed = report.freshly_keyed_hostnames.len(),
                        "key resolution"
                    );
                    tracing::debug!(
                        reused = ?report.reused_hostnames,
                        freshly_keyed = ?report.freshly_keyed_hostnames,
                        "key resolution detail"
                    );
                    if report.published {
                        tracing::info!(
                            generation = ?report.new_generation_id,
                            uploaded = report.uploaded_hostnames.len(),
                            "published a new generation"
                        );
                    } else {
                        tracing::info!("no change; nothing published");
                    }
                    if !report.cleanup_deleted.is_empty()
                        || !report.cleanup_skipped_within_grace.is_empty()
                    {
                        tracing::info!(
                            deleted = ?report.cleanup_deleted,
                            skipped_within_grace = report.cleanup_skipped_within_grace.len(),
                            "retention cleanup"
                        );
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
