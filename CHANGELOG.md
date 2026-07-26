# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.6](https://github.com/anatta-rs/repolith/compare/repolith-core-v0.0.5...repolith-core-v0.0.6) - 2026-07-26

### Added

- *(actions)* docker build action — build-only, two-stage path containment (closes #61) ([#66](https://github.com/anatta-rs/repolith/pull/66))

## [0.0.5](https://github.com/anatta-rs/repolith/compare/v0.0.4...v0.0.5) - 2026-07-25

### Other

- *(readme)* manifest reference + crates.io install path + purge stale CI note (closes #58) ([#59](https://github.com/anatta-rs/repolith/pull/59))

## [0.0.4] - 2026-05-14

Docs-only patch release. No code, no API surface changes — just a README
polish ahead of the public open.

### Docs

- **README polish ([#55])** — landing-page rewrite:
  - centered header with 5 badges (CI · License · Rust 1.85+ · v0.0.4 · PRs welcome);
  - tagline (`⚡ Parallel · 🛑 Cancellation-aware · 💾 Cache-first`);
  - new "What it looks like" section with a real terminal demo
    (status → sync --dry-run → sync → no-op sync);
  - new "Why use it" section with 5 concrete bullets, including the
    hardened-argv story added in v0.0.3;
  - "What it is NOT" hoisted before "Quick start" (the question most
    readers have before they install anything);
  - clickable crate table linking to `crates/<name>` directories;
  - Status block bumped to v0.0.4 + one-line note about the three
    pre-public audit cycles that closed all must-fix items;
  - new top-level "Security" section pointing at SECURITY.md.
- **Supersedes PR [#30]** — the open README-polish PR predated the audit
  cycles and conflicted on every facts-bearing line (test count, version,
  removed `init` subcommand). #55 absorbs the original design intent
  (badges, terminal demo, hoisted is-NOT, clickable crates) fresh against
  v0.0.3.

### Closed PRs

[#30] · [#55]

[#30]: https://github.com/anatta-rs/repolith/pull/30
[#55]: https://github.com/anatta-rs/repolith/pull/55

## [0.0.3] - 2026-05-13

The pre-public-open polish release. Two audit cycles on top of the v0.0.2
tag surfaced 20 must-fix items across 6 PRs; this release rolls them up.
No new features; no API breakage for the CLI; one breaking change for
direct embedders of `repolith-engine` (see below).

### Fixed (security / correctness)

- **`kill_group` probe semantics ([#51])** — the SIGTERM/SIGKILL escalation
  on Unix used `kill(pgid, None)` to check whether to escalate. That probes
  the *leader* process, not the *group* — so when the leader exited before
  the grace elapsed but a stubborn grandchild (rustc, ssh, mold) was still
  alive, the SIGKILL was skipped and the grandchild leaked. Fix: probe the
  group via `killpg(pgid, None)`. Regression test reproduces the case.
- **`kill_group` no longer detached ([#47])** — the SIGKILL escalation was
  a fire-and-forget `tokio::spawn`; if the runtime tore down before the
  250 ms grace elapsed (CLI exit on a no-op layer), the SIGKILL never
  fired. Now an `async fn` awaited inside `run_with_cancel`.
- **`kill_group` PID-recycle guard ([#47])** — before SIGKILL, the function
  now checks group liveness so the OS recycling the pgid during the grace
  doesn't signal an unrelated group.
- **`Orchestrator::builder()` default env is empty ([#47]; breaking for
  SDK embedders)** — was `std::env::vars().collect()`. The CLI's
  `filtered_env()` saved the binary, but any direct engine embedder that
  forgot `.base_ctx(...)` inherited a `Ctx` snapshot containing
  `GITHUB_TOKEN` / `AWS_SECRET_ACCESS_KEY`. **This is a behaviour change
  for `repolith-engine` consumers** — pass an explicit `Ctx::env` to
  preserve the prior pass-through.
- **`GIT_SSH_COMMAND` removed from `ENV_ALLOWLIST` ([#51])** — git
  shell-evaluates this on every invocation, so passing it through means
  a hostile parent env owns subprocess execution. `SSH_AUTH_SOCK` (a UNIX
  socket path) stays.
- **`check_url` rejects nested-URL leading dashes + control chars ([#39])**
  — `ssh://-oProxyCommand=evil@host/r.git` used to pass the position-0
  dash check. Now `userinfo`, `host`, and every path segment are walked
  via `url::Url::parse` (with a hand-rolled SCP-style branch for
  `git@host:org/repo`), and any embedded control char is rejected.
- **`--` argv separator before user URLs in `git ls-remote` / `git clone`
  ([#39])** — defense in depth even if `check_url` ever regresses.
- **Empty `git ls-remote` rejected ([#41])** — `git ls-remote HEAD` exit-0
  with empty stdout (repo with no HEAD) used to produce a stable hash
  over the empty SHA1 → action looked "up to date" forever. Now surfaces
  `BuildError::UpstreamUnreachable("no HEAD")`.
- **`Plan::compute` cancel during layer fan-out ([#41])** — per-action
  `input_hash` futures now race against `ctx.cancel.cancelled()`; cancel
  during a wide `git ls-remote` storm short-circuits in milliseconds.
- **Node-id path-traversal rejection ([#49], extended in [#51])** — node
  ids containing `/`, `\`, `:` (Windows drive letter / ADS), control
  characters (NUL truncates at the syscall layer), or the literal `.` /
  `..` tokens are rejected at manifest parse time. Default clone path
  `./{id}` is no longer escapable.
- **Windows `.exe` suffix ([#49])** — `cargo install` writes
  `<name>.exe` on Windows; the action now reads at the correct path via
  `std::env::consts::EXE_SUFFIX`.
- **`tokio::fs::read` for installed-binary hash ([#49])** —
  `std::fs::read` blocked the tokio worker on a multi-MB binary; with N
  parallel installs at `--jobs=num_cpus`, N workers stalled on disk.
  Now releases the worker.

### Added

- **`SSH_AUTH_SOCK`, `XDG_CONFIG_HOME` in `ENV_ALLOWLIST` ([#49])** —
  private-repo SSH clones now reach the running ssh-agent; user git
  config overrides via `$XDG_CONFIG_HOME/git/config` work.
- **Process-group cancel propagation on Unix ([#41])** — subprocess
  children are spawned in their own process group; on cancel the group
  receives `SIGTERM` then (after a 250 ms grace) `SIGKILL`, reaping
  grandchildren `kill_on_drop` alone would leave running.

### Changed

- **MSRV declared as `rust-version = "1.85"` ([#37])** — propagated to
  every member crate via `rust-version.workspace = true`. Matches
  `edition = "2024"`'s implicit floor.
- **`repolith-actions` default features are now `["git", "cargo"]`
  ([#37])** — was `[]`, which made `cargo test -p repolith-actions`
  silently compile zero tests. The library still supports
  `default-features = false` for embedders who want to opt out.
- **`init_tracing` filter includes all 4 internal crates ([#43])** —
  was `repolith` + `repolith_engine`; now also `repolith_cache` and
  `repolith_actions` so cache misses and subprocess errors are no
  longer silent at default log level.
- **Subprocess argument-injection hardening (see Fixed above) ([#39])**.

### Docs

- **README "Status" block refreshed ([#45], [#52])** — was pinned to
  "M1 (v0.0.1) shipped, ~60 tests"; now reflects v0.0.3 with a soft test
  count that doesn't drift on every new test.
- **CI status note in README ([#37])** — workflow paused on org-level
  Actions billing; README points reviewers at the canonical local
  `cargo` invocations.
- **SECURITY.md vulnerability reporting ([#45])** — was an unreachable
  pointer at `Cargo.toml authors`; now uses GitHub's private vulnerability
  advisories form. **Operators must enable "Private vulnerability
  reporting" in repo Settings → Security for this to work.**
- **SECURITY.md SIGTERM bullet ([#45])** — was marked "deferred to M2";
  now reflects that SIGINT + SIGTERM are both trapped and the Unix
  process-group reap is in place.
- **SECURITY.md env-allowlist reference ([#52])** — was a hardcoded list
  that drifted every time the source list moved; now points readers at
  `ENV_ALLOWLIST` in source.
- **`repolith-core` rustdoc + ARCHITECTURE.md no-runtime invariant
  ([#43])** — restated correctly: no `tokio` runtime dep, but lightweight
  `futures` combinators are fine.
- **CHANGELOG closed-PRs references ([#43])** — 0.0.2 entry now has
  link defs for all the audit-cycle-1 PRs, mirroring 0.0.1's style.

### Tracked + lockfile

- **`Cargo.lock` is now tracked ([#37])** — was gitignored despite the
  workspace shipping a binary. `cargo install --git` users now get a
  reproducible dep tree.

### Closed PRs

[#37] · [#39] · [#41] · [#43] · [#45] · [#47] · [#49] · [#51] · [#52]

[#37]: https://github.com/anatta-rs/repolith/pull/37
[#39]: https://github.com/anatta-rs/repolith/pull/39
[#41]: https://github.com/anatta-rs/repolith/pull/41
[#43]: https://github.com/anatta-rs/repolith/pull/43
[#45]: https://github.com/anatta-rs/repolith/pull/45
[#47]: https://github.com/anatta-rs/repolith/pull/47
[#49]: https://github.com/anatta-rs/repolith/pull/49
[#51]: https://github.com/anatta-rs/repolith/pull/51
[#52]: https://github.com/anatta-rs/repolith/pull/52

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

### Post-release audit hardening

A full second-pass audit on the v0.0.2 tag surfaced 12 must-fix items
that landed in four follow-up PRs (PRs [#37], [#39], [#41], [#43]).
They are reflected in the per-section entries above plus the prose
alignments in [#43]; none changes public API.

[#36] · [#37] · [#38] · [#39] · [#40] · [#41] · [#42] · [#43]

[#36]: https://github.com/anatta-rs/repolith/issues/36
[#37]: https://github.com/anatta-rs/repolith/pull/37
[#38]: https://github.com/anatta-rs/repolith/issues/38
[#39]: https://github.com/anatta-rs/repolith/pull/39
[#40]: https://github.com/anatta-rs/repolith/issues/40
[#41]: https://github.com/anatta-rs/repolith/pull/41
[#42]: https://github.com/anatta-rs/repolith/issues/42
[#43]: https://github.com/anatta-rs/repolith/pull/43

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
