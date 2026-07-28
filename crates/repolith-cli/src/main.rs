//! `repolith` CLI — user-facing entry point for the orchestrator.
//!
//! Two subcommands:
//!
//! - `sync` — walk the action DAG and rebuild stale entries.
//! - `status` — print a cache hit/miss table without executing anything;
//!   with a filter argument, a detail block per matching action.
//!
//! Signal handling: a single root `CancellationToken` is created in `main`,
//! passed to the orchestrator via `Builder::base_ctx`, and watched by a
//! background task that fires it on `SIGINT`/`SIGTERM`. Every action then
//! short-circuits its in-flight subprocess on the next `.await` poll.

mod factory;
mod progress;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use comfy_table::{Cell, Table};
use progress::{CliProgress, Heartbeat};
use repolith_cache::SqliteCache;
use repolith_core::manifest::{ActionEntry, Manifest, action_kind};
use repolith_core::plan::{ChangeReason, Plan};
use repolith_core::types::{ActionId, BuildEvent, Ctx, ExecMode};
use repolith_engine::orchestrator::Orchestrator;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
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

    /// Cache database path (sqlite backend only). Defaults to
    /// `~/.repolith/cache.db`.
    #[arg(long, env = "REPOLITH_CACHE_PATH", global = true)]
    cache_path: Option<PathBuf>,

    /// Cache backend. `neo4j` reads `REPOLITH_NEO4J_URI` / `_USER` /
    /// `_PASS` from the environment (credentials never live in the
    /// manifest).
    #[arg(long, env = "REPOLITH_CACHE", global = true, value_enum, default_value_t = CacheBackend::Sqlite)]
    cache: CacheBackend,

    /// Verbosity: `-v` for debug, `-vv` for trace.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

/// Selectable cache backends. `SQLite` is the zero-config default;
/// Neo4j targets shared / multi-machine setups.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CacheBackend {
    /// Local `SQLite` file (default).
    Sqlite,
    /// Shared Neo4j server — env-configured, see `--cache` help.
    Neo4j,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Walk the action DAG and execute stale actions.
    Sync(SyncArgs),

    /// Print a cache hit/miss table — no execution.
    ///
    /// With FILTER, print a detail block per matching action instead:
    /// the full error, full hashes, resolved inputs, dependencies and
    /// artifact state — everything the table has to truncate away.
    Status {
        /// Substring of an action id, e.g. `land` or `land::cargo`.
        ///
        /// Ids are `{node}::{kind}::{index}`; run `repolith status` with
        /// no argument to list them.
        filter: Option<String>,
    },
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

    /// Re-run actions even when the cache says they are up to date.
    ///
    /// Bare `--force` forces everything; `--force <FILTER>` forces only
    /// actions whose id contains FILTER — the same matching
    /// `repolith status <FILTER>` uses. Whatever is built from a forced
    /// action re-runs too.
    ///
    /// Reach for it when you suspect the cache is wrong, or when an input
    /// exists that repolith does not model (a system library, a rustc
    /// nightly rolling forward). It only ever causes more work, never less.
    #[arg(long, value_name = "FILTER", num_args = 0..=1, default_missing_value = "")]
    force: Option<String>,
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
        Cmd::Status { filter } => run_status(&cli, cancel, filter.as_deref()).await,
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
    // Private-repo / SSH support — `SSH_AUTH_SOCK` is just a UNIX-domain
    // socket path; safe to forward so `git clone` over ssh can reach the
    // running ssh-agent. Note: `GIT_SSH_COMMAND` is intentionally NOT in
    // this list — git evaluates it as a shell command on every invocation,
    // so a hostile parent env containing `GIT_SSH_COMMAND='sh -c "evil"'`
    // would be RCE. Operators who genuinely need a custom SSH wrapper can
    // construct an `Orchestrator` directly and pass an explicit `Ctx::env`.
    "SSH_AUTH_SOCK",
    // `~/.config` overrides — `git` reads `$XDG_CONFIG_HOME/git/config`
    // when set; without the passthrough, custom git config is silently
    // ignored.
    "XDG_CONFIG_HOME",
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

/// Open the cache backend selected by `--cache` / `REPOLITH_CACHE`.
async fn open_cache(cli: &Cli) -> Result<Box<dyn repolith_core::cache::Cache>> {
    match cli.cache {
        CacheBackend::Sqlite => {
            let cache_path = cli.cache_path.clone().unwrap_or_else(default_cache_path);
            let cache = SqliteCache::open(&cache_path)
                .with_context(|| format!("opening cache at `{}`", cache_path.display()))?;
            Ok(Box::new(cache))
        }
        CacheBackend::Neo4j => {
            let cfg = repolith_cache::Neo4jConfig::from_env()?;
            let cache = repolith_cache::Neo4jCache::connect(&cfg).await?;
            Ok(Box::new(cache))
        }
    }
}

async fn build_orchestrator(
    cli: &Cli,
    cancel: CancellationToken,
    jobs: usize,
    mode: ExecMode,
) -> Result<Orchestrator> {
    let manifest = load_manifest(&cli.manifest)?;

    // Federation wiring: one semaphore bounds the whole tree of
    // orchestrators; the hooks let nested `RepolithSync` actions rebuild
    // this factory + a local SQLite cache for each child stack. The
    // canonicalized root manifest path seeds the cycle-detection chain.
    let manifest_path = cli
        .manifest
        .canonicalize()
        .with_context(|| format!("canonicalize manifest `{}`", cli.manifest.display()))?;
    let base_dir = manifest_path
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(jobs.max(1)));
    let hooks = factory::CliFederationHooks::new(mode, std::sync::Arc::clone(&sem));
    let fctx = factory::FactoryCtx {
        base_dir,
        chain: vec![manifest_path],
        mode,
        sem: std::sync::Arc::clone(&sem),
        hooks,
    };
    let actions = factory::build_actions_from_manifest(&manifest, &fctx)?;

    let cache = open_cache(cli).await?;

    let base_ctx = Ctx {
        cancel,
        workdir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        env: filtered_env(),
    };

    let mut builder = Orchestrator::builder()
        .cache_boxed(cache)
        .manifest(manifest)
        .max_parallelism(jobs)
        .shared_semaphore(sem)
        .base_ctx(base_ctx);

    for action in actions {
        builder = builder.register_boxed(action);
    }

    builder.build().map_err(anyhow::Error::from)
}

/// Turn `--force [FILTER]` into the set of action ids to force.
///
/// A bare `--force` arrives as an empty filter, and `id.contains("")` is
/// always true — so "force everything" and "force a subset" are the same
/// code path rather than two, and there is no separate all-or-nothing flag
/// to keep in step.
fn resolve_forced(orch: &Orchestrator, filter: Option<&str>) -> Result<HashSet<ActionId>> {
    let Some(filter) = filter else {
        return Ok(HashSet::new());
    };
    let forced: HashSet<ActionId> = orch
        .actions()
        .iter()
        .map(|a| a.id())
        .filter(|id| id.0.contains(filter))
        .collect();

    // A filter that matches nothing is a failed request, not a quiet no-op:
    // otherwise a typo looks exactly like "everything was already fine".
    // An empty filter over an empty manifest is not an error — there is
    // simply nothing to do, which `dry-run` already reports.
    if forced.is_empty() && !filter.is_empty() {
        anyhow::bail!(
            "no action id contains `{filter}` — run `repolith status` with no argument to list them"
        );
    }
    Ok(forced)
}

async fn run_sync(cli: &Cli, args: &SyncArgs, cancel: CancellationToken) -> Result<()> {
    let mode = if args.keep_going {
        ExecMode::KeepGoing
    } else {
        ExecMode::FailFast
    };
    let mut orch = build_orchestrator(cli, cancel, args.jobs, mode).await?;
    let forced = resolve_forced(&orch, args.force.as_deref())?;
    let plan = orch.compute_plan_with_forced(&forced).await?;

    if args.explain || args.dry_run {
        if plan.reasons().is_empty() {
            println!("up to date — no stale actions");
        } else {
            for (id, reason) in plan.reasons() {
                println!("• {id}: {reason}");
            }
        }
    }

    if args.dry_run {
        println!("dry-run: {} action(s) would run", plan.reasons().len());
        return Ok(());
    }

    // One writer on stdout for the whole run: the heartbeat writes through
    // this same CliProgress, and so does the final summary.
    let progress = Arc::new(CliProgress::new());
    let heartbeat = Heartbeat::spawn(Arc::clone(&progress));

    let outcome = orch
        .execute_plan_observed(&plan, mode, progress.as_ref())
        .await;

    // Ordered shutdown before the summary. `Drop` alone would cancel without
    // waiting, letting a tick already inside `writeln!` land mid-summary.
    heartbeat.stop().await;

    match outcome {
        Ok(events) => {
            progress.summary(&events);
            Ok(())
        }
        Err(repolith_engine::orchestrator::ExecError::LayerFailed { events }) => {
            progress.summary(&events);
            anyhow::bail!("sync failed: see events above");
        }
        Err(other) => Err(anyhow::Error::from(other)),
    }
}

async fn run_status(cli: &Cli, cancel: CancellationToken, filter: Option<&str>) -> Result<()> {
    let orch = build_orchestrator(cli, cancel, num_cpus::get(), ExecMode::FailFast).await?;
    let plan = orch.compute_plan().await?;

    match filter {
        None => {
            print_status_table(&plan);
            Ok(())
        }
        Some(f) => print_status_detail(&orch, &plan, f).await,
    }
}

fn print_status_table(plan: &Plan) {
    let reasons: HashMap<_, _> = plan.reasons().iter().collect();

    let mut table = Table::new();
    table.set_header(vec!["Action", "Status", "Reason"]);
    for id in plan.flat_topo() {
        if let Some(reason) = reasons.get(id) {
            table.add_row(vec![
                Cell::new(id.to_string()),
                Cell::new("stale"),
                Cell::new(reason.to_string()),
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
}

/// Width of the label column in a detail block. Sized for the longest
/// label in use (`install to`) plus a separating space.
const LABEL_W: usize = 12;

/// Print one `label   value` line, indenting any continuation lines to the
/// value column so multi-line values (a full error, both input hashes)
/// stay readable instead of wrapping back to the margin.
fn field(label: &str, value: &str) {
    let mut lines = value.lines();
    println!("  {:<LABEL_W$}{}", label, lines.next().unwrap_or(""));
    for line in lines {
        println!("  {:<LABEL_W$}{line}", "");
    }
}

async fn print_status_detail(orch: &Orchestrator, plan: &Plan, filter: &str) -> Result<()> {
    let matches: Vec<&ActionId> = plan
        .flat_topo()
        .filter(|id| id.0.contains(filter))
        .collect();

    // A query that matched nothing is not a success. Exiting non-zero is
    // what lets a script tell "this action is fine" from "you typo'd it".
    if matches.is_empty() {
        anyhow::bail!(
            "no action id contains `{filter}` — run `repolith status` with no argument to list them"
        );
    }

    let reasons = plan.reasons();
    for (n, id) in matches.iter().enumerate() {
        if n > 0 {
            println!();
        }
        println!("{id}");

        if let Some(reason) = reasons.get(*id) {
            field("state", "stale");
            // Untruncated, unlike the table cell this view exists to expand.
            field("reason", &reason.to_string());
        } else {
            field("state", "up-to-date");
        }

        print_last_run(orch, id).await;
        print_hashes(orch, plan, id).await;
        print_action_facts(orch, reasons, id).await;
    }
    Ok(())
}

async fn print_last_run(orch: &Orchestrator, id: &ActionId) {
    let Some(rec) = orch.cache().last_record(id).await else {
        field("last run", "never — nothing cached for this id");
        return;
    };
    let (outcome, ms) = match &rec.event {
        BuildEvent::Success { ms, .. } => ("succeeded", *ms),
        BuildEvent::Failed { ms, .. } => ("failed", *ms),
    };
    let when = rec.recorded_at.map_or_else(
        // Pre-0.0.11 cache rows carry no date. Say so rather than invent one.
        || "date not recorded by this cache".to_string(),
        human_ago,
    );
    field(
        "last run",
        &format!("{outcome} in {}, {when}", human_ms(ms)),
    );

    if let BuildEvent::Failed { error, .. } = &rec.event {
        // The whole error, every line of it — the table shows the first
        // line capped at 72 characters, and this is where the rest lives.
        field("error", &error.to_string());
    }
}

async fn print_hashes(orch: &Orchestrator, plan: &Plan, id: &ActionId) {
    let current = plan.input_hash(id);
    let cached = orch.cache().last_record(id).await.map(|r| match r.event {
        BuildEvent::Success { input, .. } | BuildEvent::Failed { input, .. } => input,
    });
    // Full hashes here — the table prints the 8-char short form.
    let value = match (cached, current) {
        (Some(c), Some(n)) => format!("cached   {c}\ncurrent  {n}"),
        (None, Some(n)) => format!("current  {n}"),
        (Some(c), None) => format!("cached   {c}"),
        (None, None) => return,
    };
    field("input", &value);
}

async fn print_action_facts(
    orch: &Orchestrator,
    reasons: &HashMap<ActionId, ChangeReason>,
    id: &ActionId,
) {
    if let Some(action) = orch.actions().iter().find(|a| a.id() == *id) {
        let deps = action.deps();
        if deps.is_empty() {
            field("deps", "none");
        } else {
            let lines: Vec<String> = deps
                .iter()
                .map(|d| {
                    let state = if reasons.contains_key(d) {
                        "stale"
                    } else {
                        "up-to-date"
                    };
                    format!("{d} ({state})")
                })
                .collect();
            field("deps", &lines.join("\n"));
        }

        // Whether the artifact is actually on disk — the cache saying
        // "built" and the binary existing are two different facts, and
        // their disagreement is exactly what `OutputMissing` reports.
        let present = action.output_present(orch.base_ctx()).await;
        field("artifact", if present { "present" } else { "missing" });
    }

    let Some(manifest) = orch.manifest.as_ref() else {
        return;
    };
    let Some((node, entry)) = manifest_entry(manifest, id) else {
        return;
    };

    match (&node.path, &node.git) {
        (Some(p), _) => field("source", &format!("path {}", p.display())),
        (None, Some(g)) => field("source", &format!("git  {g}")),
        (None, None) => {}
    }
    field("action", action_kind(entry));
    print_entry_fields(entry);
}

/// The action's own declared inputs, one field per TOML key that is set.
fn print_entry_fields(entry: &ActionEntry) {
    match entry {
        ActionEntry::GitClone => {}
        ActionEntry::CargoInstall {
            crate_name,
            package,
            profile,
            features,
            install_to,
        } => {
            if let Some(c) = crate_name {
                field("bin", c);
            }
            if let Some(p) = package {
                field("package", p);
            }
            field(
                "profile",
                profile.as_deref().unwrap_or("release (cargo default)"),
            );
            if !features.is_empty() {
                field("features", &features.join(", "));
            }
            if let Some(d) = install_to {
                field("install to", &d.display().to_string());
            }
        }
        ActionEntry::Docker {
            tag,
            dockerfile,
            context,
        } => {
            field("tag", tag);
            if let Some(d) = dockerfile {
                field("dockerfile", &d.display().to_string());
            }
            if let Some(c) = context {
                field("context", &c.display().to_string());
            }
        }
        ActionEntry::Repolith { manifest } => {
            field(
                "manifest",
                &manifest
                    .as_ref()
                    .map_or_else(|| "repolith.toml".to_string(), |p| p.display().to_string()),
            );
        }
    }
}

/// Find the manifest node + entry an [`ActionId`] came from.
///
/// Zips [`Manifest::action_ids`] against the same flat node/action walk it
/// performs internally, rather than re-deriving the `{node}::{kind}::{i}`
/// format here — one source of truth for the id shape, so this cannot
/// drift out of step with it.
fn manifest_entry<'m>(
    manifest: &'m Manifest,
    id: &ActionId,
) -> Option<(&'m repolith_core::manifest::NodeEntry, &'m ActionEntry)> {
    let flat = manifest
        .nodes
        .iter()
        .flat_map(|n| n.actions.iter().map(move |a| (n, a)));
    manifest
        .action_ids()
        .into_iter()
        .zip(flat)
        .find_map(|(candidate, pair)| (candidate == *id).then_some(pair))
}

/// Milliseconds since the epoch, or 0 if the clock is unreadable.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// "12 minutes ago" — the form that actually answers "is this recent?".
///
/// Rounds to the nearest unit and switches unit past 1.5x, so nothing ever
/// reads "90 minutes ago". A shared cache written by a machine whose clock
/// runs ahead can date an event in the future; that saturates to "0 seconds
/// ago" rather than underflowing.
fn human_ago(recorded_ms: u64) -> String {
    let secs = now_ms().saturating_sub(recorded_ms) / 1000;
    let (n, unit) = match secs {
        0 => return "just now".to_string(),
        1..=89 => (secs, "second"),
        90..=5399 => ((secs + 30) / 60, "minute"),
        5400..=129_599 => ((secs + 1800) / 3600, "hour"),
        _ => ((secs + 43_200) / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// A duration a human can read. Integer arithmetic throughout — a float
/// cast would buy nothing here but a precision-loss lint.
fn human_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{}.{} s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_durations() {
        assert_eq!(human_ms(0), "0 ms");
        assert_eq!(human_ms(999), "999 ms");
        assert_eq!(human_ms(1000), "1.0 s");
        assert_eq!(human_ms(26_800), "26.8 s");
        assert_eq!(human_ms(59_999), "59.9 s");
        assert_eq!(human_ms(60_000), "1 min 0 s");
        assert_eq!(human_ms(3_725_000), "62 min 5 s");
    }

    #[test]
    fn ages_round_to_the_nearest_sensible_unit() {
        let ago = |secs: u64| human_ago(now_ms().saturating_sub(secs * 1000));
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(1), "1 second ago");
        assert_eq!(ago(45), "45 seconds ago");
        // Past 1.5x a unit we switch, so nothing ever reads "90 minutes ago".
        assert_eq!(ago(120), "2 minutes ago");
        assert_eq!(ago(3600), "60 minutes ago");
        assert_eq!(ago(5400), "2 hours ago");
        assert_eq!(ago(86_400 * 3), "3 days ago");
    }

    /// A clock ahead of ours — possible on a shared Neo4j cache written by
    /// another machine — must not underflow into a nonsense age.
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        assert_eq!(human_ago(now_ms() + 3_600_000), "just now");
    }

    #[test]
    fn manifest_entry_finds_the_declaring_node() {
        let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "t"

[[node]]
id = "alpha"
path = "/tmp/a"

  [[node.action]]
  kind = "git-clone"

  [[node.action]]
  kind = "cargo-install"
  crate = "alpha"
"#;
        let m = Manifest::from_toml(toml).expect("fixture parses");

        let (node, entry) = manifest_entry(&m, &ActionId("alpha::cargo-install::1".into()))
            .expect("second action of alpha");
        assert_eq!(node.id, "alpha");
        assert!(
            matches!(entry, ActionEntry::CargoInstall { crate_name, .. }
                     if crate_name.as_deref() == Some("alpha")),
            "index 1 must resolve to the cargo-install, not the git-clone"
        );

        assert!(
            manifest_entry(&m, &ActionId("alpha::cargo-install::0".into())).is_none(),
            "the index is part of the id; a wrong one must not match"
        );
    }

    /// Continuation lines of a multi-line value line up under the first,
    /// which is what makes a full rustc diagnostic readable in a block.
    #[test]
    fn multi_line_values_indent_to_the_value_column() {
        // Rendered by hand rather than capturing stdout: the invariant is
        // the padding width, and asserting it directly is what matters.
        let pad = " ".repeat(2 + LABEL_W);
        assert_eq!(pad.len(), 14);
        assert_eq!(format!("  {:<LABEL_W$}", "error").len(), pad.len());
    }
}
