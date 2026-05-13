<div align="center">

# repolith

**Declarative orchestrator for Rust toolchains spread across multiple sibling git repositories.**

⚡ Parallel · 🛑 Cancellation-aware · 💾 Cache-first

[![CI](https://img.shields.io/github/actions/workflow/status/anatta-rs/repolith/ci.yml?branch=main&label=CI&logo=github)](https://github.com/anatta-rs/repolith/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/status-M1%20shipped-success)](CHANGELOG.md)
[![PRs welcome](https://img.shields.io/badge/PRs-welcome-brightgreen)](CONTRIBUTING.md)

</div>

`repolith` reads one `repolith.toml` describing remote repositories and the
actions to run against them — `git clone`, `cargo install`, more in M2 — then
executes the plan in **parallel layers**, with **content-addressed caching**
so untouched work never re-runs and a shared **`CancellationToken`** so
`Ctrl-C` cleanly aborts in-flight subprocesses.

## What it looks like

Given this manifest:

```toml
[orchestrator]
schema_version = "0.1"
name = "my-stack"

[[node]]
id = "a"
git = "file:///path/to/upstream-a"
path = "./clones/a"
  [[node.action]]
  kind = "git-clone"

[[node]]
id = "b"
path = "./libs/shared"
  [[node.action]]
  kind = "cargo-install"
  crate = "lib-bin-1"
  install_to = "~/.local/bin"

[[node]]
id = "c"
path = "./libs/shared"
  [[node.action]]
  kind = "cargo-install"
  crate = "lib-bin-2"
  install_to = "~/.local/bin"
```

You preview, then sync:

```console
$ repolith status
+---------------------+--------+---------------+
| Action              | Status | Reason        |
+==============================================+
| a::git-clone::0     | stale  | NoCachedBuild |
|---------------------+--------+---------------|
| b::cargo-install::0 | stale  | NoCachedBuild |
|---------------------+--------+---------------|
| c::cargo-install::0 | stale  | NoCachedBuild |
+---------------------+--------+---------------+

$ repolith sync --dry-run --explain
• a::git-clone::0: NoCachedBuild
• b::cargo-install::0: NoCachedBuild
• c::cargo-install::0: NoCachedBuild
dry-run: 3 action(s) would run

$ repolith sync
OK   a::git-clone::0 (55 ms)
OK   b::cargo-install::0 (188 ms)
OK   c::cargo-install::0 (262 ms)

$ repolith sync --dry-run
up to date — no stale actions
dry-run: 0 action(s) would run
```

The second `sync` is a **no-op** — the SQLite cache holds last-run input
hashes for each action, so nothing re-runs unless the upstream HEAD or a
cargo feature changed.

## Why use it

- **One declarative file.** Everything your stack needs to bootstrap, in
  `repolith.toml`. No bash glue, no per-machine README dance.
- **Parallel by default.** `tokio::FuturesUnordered` + `Semaphore` cap concurrency
  at `--jobs N` (default = `num_cpus`). Layer N+1 starts only when layer N
  settles — typed dependencies, no race conditions.
- **Cancels cleanly.** A shared `CancellationToken` plumbed through every
  action's `Ctx`. First failure in `--fail-fast` mode (default) cancels
  in-flight peers; `--keep-going` lets the layer finish then halts.
- **Cache-first.** Every successful build writes a `BuildEvent` to a SQLite
  store keyed by content-addressed input hashes. Re-runs are near-instant
  when nothing changed.
- **Typed errors all the way down.** `BuildError::CommandFailed { exit_code,
  stderr }` survives the cache roundtrip via JSON, so retry policies and
  diagnostics get structured failure data, not stringified blobs.

## What it is NOT

- **Not a CI runner** — no distributed execution, no remote workers.
- **Not a toolchain manager** — `rustup` is fine.
- **Not a package manager** — `cargo` is fine.
- **Not a process supervisor** — `systemd` / `launchd` / `docker compose`
  are fine; repolith reads heartbeats, doesn't write them.
- **Not a monorepo tool** — `cargo workspaces` already covers single-repo
  workspace publishing.
- **Not a hermetic build system** — hermeticity is opt-in per action, not
  the default.

## Quick start

```bash
# 1. Clone repolith
git clone https://github.com/anatta-rs/repolith && cd repolith

# 2. Build the binary
cargo build --release
cp target/release/repolith ~/.local/bin/   # or any dir on $PATH

# 3. Drop the example into your stack root, edit it
mkdir -p ../my-stack && cd ../my-stack
cp ../repolith/repolith.toml.example ./repolith.toml
$EDITOR repolith.toml

# 4. Preview what would happen
repolith sync --dry-run --explain

# 5. Go
repolith sync
```

See [`repolith.toml.example`](repolith.toml.example) for a full annotated
manifest.

## Architecture

5 crates, 3 traits (`Action`, `Source`, `Cache`), layered execution with
`FuturesUnordered` + `CancellationToken` + `Semaphore`. Full diagram +
the `FailFast` / `KeepGoing` sequence diagrams + design decisions live
in [**`ARCHITECTURE.md`**](ARCHITECTURE.md).

| Crate | Purpose |
|---|---|
| [`repolith-core`](crates/repolith-core) | Types, traits, manifest parser, layered `Plan`. |
| [`repolith-cache`](crates/repolith-cache) | `SqliteCache` (rusqlite, bundled). |
| [`repolith-engine`](crates/repolith-engine) | Async `Orchestrator` with cancellation + semaphore. |
| [`repolith-actions`](crates/repolith-actions) | `GitClone` (feature `git`), `CargoInstall` (feature `cargo`). |
| [`repolith-cli`](crates/repolith-cli) | The `repolith` binary. |

## Status

**v0.0.1 (M1) — bootstrap shipped.** 5 crates, 2 builtin actions, parallel
layered execution with cancellation, SQLite cache, 55 workspace tests,
CI on every PR. See [`CHANGELOG.md`](CHANGELOG.md) for the per-issue
breakdown.

### Roadmap

- **M2** — federation `kind = "repolith"` (orchestrator-of-orchestrators),
  Neo4j cache backend, `docker` action.
- **M3** — watch mode (re-plan on file change), `template_apply` action
  driving `AttachedEntry::Outbound`.

## Contributing

PRs welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev setup,
local CI gates, and worked recipes for adding a new action or a new cache
backend.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
