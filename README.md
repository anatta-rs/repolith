# repolith

> Declarative orchestrator for Rust toolchains spread across multiple
> sibling git repositories. Lightweight. Cache-aware. Cancellation-aware.

`repolith` reads a single `repolith.toml` describing a graph of remote
repositories and the actions to run against each of them — `git clone`,
`cargo install`, more in M2 — then executes the resulting plan in
parallel layers, with content-addressed caching so untouched work
never re-runs.

## Quick start

```bash
# 1. Clone repolith
git clone https://github.com/anatta-rs/repolith && cd repolith

# 2. Build the binary
cargo build --release
cp target/release/repolith ~/.local/bin/   # or any dir on $PATH

# 3. Copy the example into the directory where you want sibling
#    clones + tool binaries to land, then edit it for your stack
mkdir -p ../my-stack && cd ../my-stack
cp ../repolith/repolith.toml.example ./repolith.toml
$EDITOR repolith.toml

# 4. See what would happen — without doing anything
repolith sync --dry-run --explain

# 5. Go
repolith sync
```

`repolith status` prints a cache hit/miss table without running
anything. `repolith sync -k` keeps a layer running after a failure
(useful for surfacing every failure of a layer in one pass).

See [`repolith.toml.example`](repolith.toml.example) for a full
manifest with three nodes and one attached project.

## What repolith does

- **Reads a single declarative manifest** (`repolith.toml`) and turns
  it into a typed `Action` DAG.
- **Plans before executing.** `compute_plan` walks the graph
  topologically (Kahn) and decides which actions are stale by
  comparing each action's input hash against the last persisted
  result.
- **Executes layers in parallel** via `tokio::FuturesUnordered` +
  `Semaphore`, capped by `--jobs N` (default = `num_cpus`).
- **Cancels in-flight subprocesses on first failure** in `--fail-fast`
  mode (default), via a shared `CancellationToken` plumbed through
  every action's `Ctx`. `--keep-going` lets the current layer settle.
- **Persists every event** to a SQLite cache so the next `sync` is a
  near-no-op when nothing changed upstream.

## What repolith does NOT do

The negative scope is **fixed** and will not evolve:

- Not a CI runner — no distributed execution, no remote workers.
- Not a toolchain manager — `rustup` is fine.
- Not a package manager — `cargo` is fine.
- Not a process supervisor — `systemd` / `launchd` /
  `docker compose` are fine; repolith reads heartbeats, doesn't
  write them.
- Not a monorepo tool — `cargo workspaces` already covers single-repo
  workspace publishing.
- Not a hermetic build system — hermeticity is opt-in per action,
  not the default.

## Architecture

5 crates, 3 traits, layered execution with `FuturesUnordered` +
`CancellationToken` + `Semaphore`. Full diagram + the two
`FailFast` / `KeepGoing` sequence diagrams + design decisions live
in [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Status

**M1 — bootstrap (v0.0.1) shipped.** 5 crates, 2 builtin actions,
parallel layered execution with cancellation, SQLite cache, 55
workspace tests, CI on every PR.

| Crate | Purpose |
|---|---|
| `repolith-core` | Types, traits (`Action`, `Source`, `Cache`), manifest parser, layered `Plan`. |
| `repolith-cache` | `SqliteCache` (rusqlite, bundled). |
| `repolith-engine` | Async `Orchestrator` with `FuturesUnordered` + `CancellationToken` + `Semaphore`. |
| `repolith-actions` | `GitClone` (feature `git`), `CargoInstall` (feature `cargo`). |
| `repolith-cli` | `repolith init / sync / status` — the binary you run. |

See [`CHANGELOG.md`](CHANGELOG.md) for the per-issue breakdown.

### Roadmap

- **M2** — federation `kind = "repolith"` (orchestrator-of-orchestrators),
  Neo4j cache backend, `docker` action.
- **M3** — watch mode (re-plan on file change), `template_apply`
  action driving `AttachedEntry::Outbound`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev setup, test
workflow, and recipes for adding a new action or a new cache backend.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
