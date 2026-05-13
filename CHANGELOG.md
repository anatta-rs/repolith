# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.1] - 2026-05-13

### Added — M1 bootstrap

The first end-to-end runnable cut of repolith. Every layer wired
together, smoke test exercising the full pipeline, and CI guarding
against regressions.

#### Crates
- **`repolith-core`** ([#1], [#2], [#3], [#4], [#5]) — types
  (`ActionId`, `Sha256`, `BuildEvent`, `Ctx`, `BuildError`, `ExecMode`),
  traits (`Action`, `Source`, `Cache`), manifest parser with `~0.1`
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
