# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.2] - 2026-05-13

### Removed (BREAKING)

These public types were unused or premature in M1 and are gone in 0.0.2.
Pre-1.0 cleanup; restored when the M2 features that need them land.

- `repolith_core::source` module + `Source` trait (no implementor in M1).
- `repolith_core::manifest::AttachedEntry` and `Direction` enum
  (`Manifest.attached` field too). `[[attached]]` blocks in
  `repolith.toml` are still parsed for forward compatibility but
  ignored. The schema returns with the M2 `template_apply` action.
- CLI `repolith init` subcommand. It was a thin alias for `sync` with
  default flags; users should call `repolith sync` directly.

### Changed

- `BuildError` Display strings now use the lowercase-prefix convention
  shared with `CacheError` and `PlanError` (`command failed (exit N): …`,
  `io: …`, `build cancelled`, `upstream unreachable: …`). Any tool that
  greps logs for the old `Command failed:` / `IO error:` / `Build cancelled`
  strings will need to update its patterns.
- `repolith_core::manifest::action_kind` is now `pub` so the CLI's
  manifest factory can reuse it instead of duplicating the
  `git-clone`/`cargo-install` mapping. No semantic change.

### Added

- SIGTERM handler in addition to SIGINT (CLI now traps both signals
  and propagates a single cancel through the orchestrator).
- `Cache::record_batch` trait method (default impl loops `record`;
  `SqliteCache` overrides with a transaction so per-layer event
  writes are atomic).
- Argument-injection validation at manifest parse time
  (`ManifestError::InvalidUrl` and `InvalidArg`).
- `SECURITY.md` with the M1 threat model and known limitations.

### Fixed

- `cargo_install` was reading the installed binary at the unexpanded
  `install_to` path while cargo wrote at the expanded path; every
  `~/...` install now succeeds.
- `SqliteCache` now opens with `journal_mode = WAL`,
  `synchronous = NORMAL`, and `busy_timeout = 5s` so concurrent
  `repolith sync` invocations don't deadlock with `database is locked`.
- Subprocesses spawned by actions are now killed when the wrapping
  future is dropped (via `Command::kill_on_drop(true)`); previously
  Ctrl-C left orphan `git`/`cargo` processes.
- Workspace-package metadata (license, description, repository, etc.)
  is now properly inherited by every member crate so `cargo publish`
  works.
- `Plan::compute` now runs per-layer `input_hash` and `cache.last_build`
  probes concurrently; was sequential, scaled poorly with N nodes.
- `Plan::compute` and `Orchestrator::execute_plan` now poll
  `ctx.cancel` between layers so a Ctrl-C during long network probes
  short-circuits cleanly.
- `Ctx.env` now carries an allowlisted subset of the parent process
  env instead of the full snapshot, so secrets like `GITHUB_TOKEN` and
  `AWS_SECRET_ACCESS_KEY` can no longer leak through a future
  `tracing::debug!(?ctx)` or panic dump.
- Tilde expansion (`~/...`) is now applied to `node.path` in addition
  to `install_to`, so manifests using `~`-prefixed sibling clone paths
  no longer create literal `~` directories.

## [0.0.1] - 2026-05-13

### Added — M1 bootstrap

The first end-to-end runnable cut of repolith. Every layer wired
together, smoke test exercising the full pipeline, and CI guarding
against regressions.

#### Crates
- **`repolith-core`** ([#1], [#2], [#3], [#4], [#5]) — types
  (`ActionId`, `Sha256`, `BuildEvent`, `Ctx`, `BuildError`, `ExecMode`),
  traits (`Action`, `Cache`), manifest parser with `~0.1`
  schema validation + duplicate-id + missing-source checks, layered
  `Plan` (Kahn topological sort) with cascading `ChangeReason`
  (`NoCachedBuild` / `InputHashChanged` / `UpstreamMoved`).
- **`repolith-cache`** ([#7]) — `SqliteCache` via `rusqlite` (bundled),
  events stored with their typed `BuildError` JSON-serialized so
  `BuildEvent::Failed` survives the roundtrip losslessly.
- **`repolith-engine`** ([#6]) — async `Orchestrator` + `Builder`,
  `FuturesUnordered` + `CancellationToken` + `Semaphore` (no
  `tokio::join_all` — would defeat `FailFast`). `FailFast` cancels
  in-flight peers; `KeepGoing` settles the layer; both halt before
  the next layer on any failure.
- **`repolith-actions`** ([#8], [#9]) — `GitClone` (feature `git`)
  shells out to the `git` CLI; `CargoInstall` (feature `cargo`) wraps
  `cargo install --bin <name> --locked --force --root <dir>`.
  Subprocesses race against `ctx.cancel`. `~/...` in `install_to`
  expands via `dirs::home_dir()`.
- **`repolith-cli`** ([#10]) — `repolith init | sync | status` with
  global `--manifest`, `--cache-path` (env `REPOLITH_CACHE_PATH`),
  `-v` verbosity. `sync` flags: `-j N`, `-k/--keep-going`,
  `--explain`, `--dry-run`. `status` renders a 3-column
  `comfy-table`. `tokio::signal::ctrl_c` fires the root
  `CancellationToken` so SIGINT cancels in-flight subprocesses.

#### Tests ([#11])
- 55 workspace tests across unit + integration, including a
  black-box smoke test that shells out to the binary against a
  realistic fixture (`git init` + commit + cargo install).

#### CI ([#12])
- Single ubuntu-latest job: `cargo fmt --check`, `cargo clippy
  --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace --all-features`, `cargo doc` with
  `RUSTDOCFLAGS=-D warnings`. Caching via `Swatinem/rust-cache@v2`.

#### Docs ([#13])
- `repolith.toml.example` at the repo root demonstrates a typical
  stack (4 nodes + 1 attached) with placeholder URLs. The example
  is parsed by an integration test so docs and code can't drift.
- README "Quick start" with 5 copy-paste steps.

### Architecture decisions
- **`Cache` trait in `repolith-core`** — `Plan::compute` consumes
  `&dyn Cache`; the trait sits in `core` so `engine` and `cache`
  can both depend on it without a cycle. Concrete backends
  (`SqliteCache`, `MockCache`) live in `repolith-cache`.
- **Separate `repolith-engine` crate** — keeps `repolith-core` a
  pure types/traits crate without the `tokio` + `futures` runtime
  dependency footprint. Mirrors the ecosystem convention
  (`axum-core` / `axum`, `tower` / `tower-http`).
- **`repolith-cache` not `repolith-cache-sqlite`** — only one backend
  in M1; splitting into `cache-<backend>` is a follow-up if/when a
  second backend appears.

### Closed issues
[#1] · [#2] · [#3] · [#4] · [#5] · [#6] · [#7] · [#8] · [#9] · [#10]
· [#11] · [#12] · [#13]

[#1]: https://github.com/anatta-rs/repolith/issues/1
[#2]: https://github.com/anatta-rs/repolith/issues/2
[#3]: https://github.com/anatta-rs/repolith/issues/3
[#4]: https://github.com/anatta-rs/repolith/issues/4
[#5]: https://github.com/anatta-rs/repolith/issues/5
[#6]: https://github.com/anatta-rs/repolith/issues/6
[#7]: https://github.com/anatta-rs/repolith/issues/7
[#8]: https://github.com/anatta-rs/repolith/issues/8
[#9]: https://github.com/anatta-rs/repolith/issues/9
[#10]: https://github.com/anatta-rs/repolith/issues/10
[#11]: https://github.com/anatta-rs/repolith/issues/11
[#12]: https://github.com/anatta-rs/repolith/issues/12
[#13]: https://github.com/anatta-rs/repolith/issues/13
