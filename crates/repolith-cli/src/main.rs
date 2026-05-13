//! `repolith` CLI — user-facing entry point for the orchestrator.
//!
//! Two subcommands:
//!
//! - `sync` — walk the action DAG and rebuild stale entries.
//! - `status` — print a cache hit/miss table without executing anything.
//!
//! Signal handling: a single root `CancellationToken` is created in `main`,
//! passed to the orchestrator via `Builder::base_ctx`, and watched by a
//! background task that fires it on `SIGINT`/`SIGTERM`. Every action then
//! short-circuits its in-flight subprocess on the next `.await` poll.

mod factory;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use comfy_table::{Cell, Table};
use repolith_cache::SqliteCache;
use repolith_core::manifest::Manifest;
use repolith_core::types::{BuildEvent, Ctx, ExecMode};
use repolith_engine::orchestrator::Orchestrator;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "repolith",
    version,
    about = "Multi-repo orchestration for Rust ecosystems"
)]
struct Cli {
    /// Path to the manifest file.
    #[arg(long, default_value = "./repolith.toml", global = true)]
    manifest: PathBuf,

    /// Cache database path. Defaults to `~/.repolith/cache.db`.
    #[arg(long, env = "REPOLITH_CACHE_PATH", global = true)]
    cache_path: Option<PathBuf>,

    /// Verbosity: `-v` for debug, `-vv` for trace.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Walk the action DAG and execute stale actions.
    Sync(SyncArgs),

    /// Print a cache hit/miss table — no execution.
    Status,
}

#[derive(clap::Args, Debug)]
struct SyncArgs {
    /// Max concurrent in-flight actions.
    #[arg(short, long, default_value_t = num_cpus::get())]
    jobs: usize,

    /// Continue executing peer actions after a failure (`ExecMode::KeepGoing`).
    #[arg(short = 'k', long)]
    keep_going: bool,

    /// Print `ChangeReason` per stale action before executing.
    #[arg(long)]
    explain: bool,

    /// Compute the plan but do not execute anything.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    // Root cancellation token — wired to ctrl_c + SIGTERM and into the
    // orchestrator's Ctx so subprocesses see the cancel on their next
    // .await poll. Either signal triggers the same cancel; whichever
    // arrives first wins, the other becomes a no-op.
    let cancel = CancellationToken::new();
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if let Some(reason) = wait_for_shutdown_signal().await {
            tracing::info!("{reason} received, cancelling in-flight actions");
            cancel_for_signal.cancel();
        }
    });

    match &cli.cmd {
        Cmd::Sync(args) => run_sync(&cli, args, cancel).await,
        Cmd::Status => run_status(&cli, cancel).await,
    }
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // Filter targets the four internal crates explicitly. `repolith` alone
    // (no underscore) catches the bin target; the three lib crates each
    // need their own entry because `tracing-subscriber` matches on the
    // exact module-path prefix, not on a wildcard.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        format!(
            "warn,repolith={level},repolith_engine={level},repolith_cache={level},repolith_actions={level}"
        )
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}

fn load_manifest(path: &PathBuf) -> Result<Manifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading manifest `{}`", path.display()))?;
    Manifest::from_toml(&text).with_context(|| format!("parsing manifest `{}`", path.display()))
}

fn default_cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".repolith")
        .join("cache.db")
}

/// Environment variables propagated into [`Ctx`]. The full process env
/// would otherwise be cloned per layer + per spawned action and could
/// leak credentials (`AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, …) through
/// any future `tracing::debug!(?ctx)` or panic dump (CWE-200).
///
/// The allowlist covers what `git`, `cargo`, and `rustup` need to find
/// their toolchains and respect locale preferences. Anything else stays
/// in the parent process env and never enters the orchestrator's data
/// flow.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "TZ",
    "LANG",
    "LC_ALL",
];

fn filtered_env() -> std::collections::HashMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| ENV_ALLOWLIST.iter().any(|allowed| *allowed == k))
        .collect()
}

/// Wait for either `SIGINT` (Ctrl-C) or `SIGTERM` (`kill <pid>`,
/// systemd / container shutdown). Returns the human-readable name of
/// whichever signal fired first, or `None` if registration fails.
///
/// Windows builds only watch `SIGINT` since `SIGTERM` is Unix-only;
/// the CLI is officially Unix-targeted but the feature gate keeps the
/// crate compilable on any platform tokio supports.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Option<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to register SIGTERM handler: {e}");
            // Fall back to ctrl_c only.
            return tokio::signal::ctrl_c().await.ok().map(|()| "SIGINT");
        }
    };
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.ok().map(|()| "SIGINT"),
        _ = term.recv() => Some("SIGTERM"),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> Option<&'static str> {
    tokio::signal::ctrl_c().await.ok().map(|()| "SIGINT")
}

fn build_orchestrator(cli: &Cli, cancel: CancellationToken, jobs: usize) -> Result<Orchestrator> {
    let manifest = load_manifest(&cli.manifest)?;
    let actions = factory::build_actions_from_manifest(&manifest)?;

    let cache_path = cli.cache_path.clone().unwrap_or_else(default_cache_path);
    let cache = SqliteCache::open(&cache_path)
        .with_context(|| format!("opening cache at `{}`", cache_path.display()))?;

    let base_ctx = Ctx {
        cancel,
        workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        env: filtered_env(),
    };

    let mut builder = Orchestrator::builder()
        .cache(cache)
        .manifest(manifest)
        .max_parallelism(jobs)
        .base_ctx(base_ctx);

    for action in actions {
        builder = builder.register_boxed(action);
    }

    builder.build().map_err(anyhow::Error::from)
}

async fn run_sync(cli: &Cli, args: &SyncArgs, cancel: CancellationToken) -> Result<()> {
    let mut orch = build_orchestrator(cli, cancel, args.jobs)?;
    let plan = orch.compute_plan().await?;

    if args.explain || args.dry_run {
        if plan.reasons().is_empty() {
            println!("up to date — no stale actions");
        } else {
            for (id, reason) in plan.reasons() {
                println!("• {id}: {reason:?}");
            }
        }
    }

    if args.dry_run {
        println!("dry-run: {} action(s) would run", plan.reasons().len());
        return Ok(());
    }

    let mode = if args.keep_going {
        ExecMode::KeepGoing
    } else {
        ExecMode::FailFast
    };

    match orch.execute_plan(&plan, mode).await {
        Ok(events) => {
            print_events(&events);
            Ok(())
        }
        Err(repolith_engine::orchestrator::ExecError::LayerFailed { events }) => {
            print_events(&events);
            anyhow::bail!("sync failed: see events above");
        }
        Err(other) => Err(anyhow::Error::from(other)),
    }
}

async fn run_status(cli: &Cli, cancel: CancellationToken) -> Result<()> {
    let orch = build_orchestrator(cli, cancel, num_cpus::get())?;
    let plan = orch.compute_plan().await?;
    let reasons: HashMap<_, _> = plan.reasons().iter().collect();

    let mut table = Table::new();
    table.set_header(vec!["Action", "Status", "Reason"]);
    for id in plan.flat_topo() {
        if let Some(reason) = reasons.get(id) {
            table.add_row(vec![
                Cell::new(id.to_string()),
                Cell::new("stale"),
                Cell::new(format!("{reason:?}")),
            ]);
        } else {
            table.add_row(vec![
                Cell::new(id.to_string()),
                Cell::new("up-to-date"),
                Cell::new("—"),
            ]);
        }
    }
    println!("{table}");
    Ok(())
}

fn print_events(events: &[BuildEvent]) {
    for ev in events {
        match ev {
            BuildEvent::Success { id, ms, .. } => println!("OK   {id} ({ms} ms)"),
            BuildEvent::Failed { id, error, ms, .. } => {
                println!("FAIL {id} ({ms} ms): {error}");
            }
        }
    }
}
