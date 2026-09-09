mod config;
mod netlink;
mod storage;
mod sync;
mod system;

use std::path::Path;
use std::time::Duration;

use clap::{Parser, Subcommand};
use fd_lock::RwLock;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "wg-client")]
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
    /// Download and apply the current published configuration for this
    /// node.
    Sync {
        #[arg(long)]
        config: std::path::PathBuf,
        /// Overrides hostname detection (--hostname > WG_CLIENT_HOSTNAME
        /// env > system hostname).
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long, conflicts_with = "daemon")]
        once: bool,
        #[arg(long, conflicts_with = "once")]
        daemon: bool,
        #[arg(long, default_value = "24h")]
        interval: String,
        #[arg(long)]
        force: bool,
    },
}

/// `60s`, `5m`, `24h`, or a plain integer number of seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num_str, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        (s, 1)
    };
    let n: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration {s:?}"))?;
    if n == 0 {
        return Err("interval must be positive".to_string());
    }
    Ok(Duration::from_secs(n * mult))
}

/// `--hostname` > `WG_CLIENT_HOSTNAME` env > system hostname
/// (wg-client.md §5). An explicitly provided but empty value from any
/// source is an error, not a fallback to the next source.
fn resolve_hostname(cli_hostname: Option<String>) -> Result<String, String> {
    let raw = if let Some(h) = cli_hostname {
        if h.is_empty() {
            return Err("--hostname must not be empty".to_string());
        }
        h
    } else if let Ok(h) = std::env::var("WG_CLIENT_HOSTNAME") {
        if h.is_empty() {
            return Err("WG_CLIENT_HOSTNAME must not be empty".to_string());
        }
        h
    } else {
        hostname::get()
            .map_err(|e| format!("failed to read system hostname: {e}"))?
            .to_string_lossy()
            .to_string()
    };
    let lower = raw.to_ascii_lowercase();
    wg_common::hostname::validate(&lower)
        .map_err(|e| format!("resolved hostname {lower:?} is invalid: {e}"))?;
    Ok(lower)
}

fn lock_path_for(iface: &str) -> std::path::PathBuf {
    Path::new("/run/wg-client").join(format!("{iface}.lock"))
}

/// Creates `/run/wg-client` (mode 0700) if it doesn't exist yet — `/run`
/// is typically tmpfs and does not survive a reboot (wg-client.md §3).
fn ensure_lock_dir() -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = Path::new("/run/wg-client");
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

async fn run_sync_pass(
    config_path: &Path,
    hostname_override: Option<String>,
    force: bool,
) -> Result<sync::SyncOutcome, String> {
    let text = std::fs::read_to_string(config_path).map_err(|e| format!("reading config: {e}"))?;
    let cfg = config::parse(&text).map_err(|e| e.to_string())?;
    let iface = config::interface_name(&cfg.conf_path).map_err(|e| e.to_string())?;
    let hostname = resolve_hostname(hostname_override)?;

    ensure_lock_dir().map_err(|e| format!("creating /run/wg-client: {e}"))?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path_for(&iface))
        .map_err(|e| format!("opening lock file: {e}"))?;
    let mut lock = RwLock::new(lock_file);
    let _guard = lock.try_write().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            format!("another wg-client instance already holds the lock for interface {iface}")
        } else {
            format!("acquiring lock: {e}")
        }
    })?;

    let storage = storage::R2ReadClient::new(&cfg.r2_config);
    let ops = system::RealSystemOps;
    let conf_path = config::conf_path_buf(&cfg.conf_path);
    Ok(sync::run_once(&storage, &ops, &hostname, &iface, &conf_path, force).await)
}

fn report(outcome: &sync::SyncOutcome) {
    if outcome.recovered_pending {
        tracing::warn!("recovered from an interrupted previous transaction");
    }
    if outcome.restored_local {
        tracing::warn!("interface was missing at pass start; restored from last-good local config");
    }
    if let Some(err) = &outcome.error {
        tracing::error!("sync failed: {err}");
        if outcome.rollback_attempted {
            tracing::error!(
                "rollback attempted, succeeded={}",
                outcome.rollback_succeeded
            );
        }
    } else if outcome.no_op {
        tracing::info!("no change");
    } else if outcome.applied {
        tracing::info!("applied new configuration");
    }
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
        Command::Sync {
            config,
            hostname,
            once: _,
            daemon,
            interval,
            force,
        } => {
            let interval = match parse_duration(&interval) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };

            if !daemon {
                return match run_sync_pass(&config, hostname, force).await {
                    Ok(outcome) => {
                        report(&outcome);
                        if outcome.error.is_some() {
                            ExitCode::FAILURE
                        } else {
                            ExitCode::SUCCESS
                        }
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                };
            }

            // Daemon mode: loop with jitter, graceful shutdown on
            // SIGTERM/SIGINT, never let a single failed pass kill the
            // service (wg-client.md §7).
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            loop {
                match run_sync_pass(&config, hostname.clone(), force).await {
                    Ok(outcome) => report(&outcome),
                    Err(e) => tracing::error!("sync pass failed: {e}"),
                }

                let jitter = Duration::from_millis(fastrand_jitter_ms(interval));
                tokio::select! {
                    _ = tokio::time::sleep(interval + jitter) => {}
                    _ = sigterm.recv() => {
                        tracing::info!("received SIGTERM, shutting down");
                        break;
                    }
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("received SIGINT, shutting down");
                        break;
                    }
                }
            }
            ExitCode::SUCCESS
        }
    }
}

/// ±10% jitter on the sleep interval so a large fleet doesn't hammer the
/// bucket in lockstep (wg-client.md §7). No extra dependency: derives a
/// pseudo-random offset from the current time.
fn fastrand_jitter_ms(interval: Duration) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let max_jitter_ms = (interval.as_millis() as u64 / 10).max(1);
    (nanos as u64) % max_jitter_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds_minutes_hours() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn rejects_zero_and_garbage() {
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn hostname_flag_takes_precedence() {
        assert_eq!(
            resolve_hostname(Some("Master-US".to_string())).unwrap(),
            "master-us"
        );
    }

    #[test]
    fn empty_hostname_flag_is_an_error_not_a_fallback() {
        assert!(resolve_hostname(Some(String::new())).is_err());
    }
}
