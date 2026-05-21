<div align="center">

# repolith

**Declarative orchestrator for Rust toolchains spread across multiple sibling git repositories.**

⚡ Parallel · 🛑 Cancellation-aware · 💾 Cache-first

[![CI](https://img.shields.io/github/actions/workflow/status/anatta-rs/repolith/ci.yml?branch=main&label=CI&logo=github)](https://github.com/anatta-rs/repolith/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Version](https://img.shields.io/badge/version-v0.0.4-success)](CHANGELOG.md)
[![PRs welcome](https://img.shields.io/badge/PRs-welcome-brightgreen)](CONTRIBUTING.md)

</div>

`repolith` reads one `repolith.toml` describing remote repositories and the
actions to run against them — `git clone`, `cargo install`, more in M2 — then
executes the plan in **parallel layers**, with **content-addressed caching**
so untouched work never re-runs and a shared **`CancellationToken`** so
`Ctrl-C` cleanly aborts every in-flight subprocess.

## What it looks like

Given a manifest:

```toml
[orchestrator]
schema_version = "0.1"
name = "my-stack"

[[node]]
id = "shared-types"
git = "https://github.com/example-org/shared-types"
path = "../libs/shared-types"
  [[node.action]]
  kind = "git-clone"

[[node]]
id = "migration-tool"
git = "https://github.com/example-org/migration-tool"
path = "../tools/migration-tool"
  [[node.action]]
  kind = "git-clone"
  [[node.action]]
  kind = "cargo-install"
  crate = "migrate"
  install_to = "~/.local/bin"
```

You preview, then sync:

```console
$ repolith status
+-------------------------------+--------+---------------+
| Action                        | Status | Reason        |
+===============================+========+===============+
| shared-types::git-clone::0    | stale  | NoCachedBuild |
| migration-tool::git-clone::0  | stale  | NoCachedBuild |
| migration-tool::cargo-install | stale  | NoCachedBuild |
+-------------------------------+--------+---------------+

$ repolith sync --dry-run --explain
• shared-types::git-clone::0: NoCachedBuild
• migration-tool::git-clone::0: NoCachedBuild
• migration-tool::cargo-install::1: NoCachedBuild
dry-run: 3 action(s) would run

$ repolith sync
OK   shared-types::git-clone::0 (55 ms)
OK   migration-tool::git-clone::0 (188 ms)
OK   migration-tool::cargo-install::1 (4.2 s)

$ repolith sync
up to date — 0 stale actions
```

The second `sync` is a **no-op** — the SQLite cache holds last-run input
hashes per action, so nothing re-runs unless an upstream HEAD or a cargo
feature actually changed.

## Why use it

- **One declarative file.** Everything your stack needs to bootstrap, in
  `repolith.toml`. No bash glue, no per-machine README dance.
- **Parallel by default.** `tokio::FuturesUnordered` + `Semaphore` cap
  concurrency at `--jobs N` (default = `num_cpus`). Layer N+1 starts only
  when layer N settles — typed dependencies, no race conditions.
- **Cancels cleanly.** A shared `CancellationToken` plumbed through every
  action's `Ctx`. First failure in `--fail-fast` (default) cancels in-flight
  peers; `--keep-going` lets the layer settle then halts. On Unix the
  subprocess process group is signalled (SIGTERM → grace → SIGKILL) so
  cargo's `rustc` / linker grandchildren get reaped too.
- **Cache-first.** Every successful build writes a `BuildEvent` to a SQLite
  store (WAL) keyed by content-addressed input hashes. Re-runs are
  near-instant when nothing changed.
- **Hardened argv.** URLs validated against a scheme allowlist with
  nested-userinfo / host / path-segment leading-dash checks; `--` argv
  separator before every user URL as defense in depth; crate names + feature
  flags rejected if they could break cargo's `--features` list.

## What it is NOT

The negative scope is **fixed** and will not evolve:

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

# 3. Drop the example into your stack root, then edit it
mkdir -p ../my-stack && cd ../my-stack
cp ../repolith/repolith.toml.example ./repolith.toml
$EDITOR repolith.toml

# 4. Preview what would happen
repolith sync --dry-run --explain

# 5. Go
repolith sync
```

`repolith status` prints a cache hit/miss table without running anything.
`repolith sync -k` keeps a layer running after a failure (useful for
surfacing every failure of a layer in one pass).

See [`repolith.toml.example`](repolith.toml.example) for a full annotated
manifest.

## Architecture

5 crates, layered execution with `FuturesUnordered` + `CancellationToken` +
`Semaphore`. Full diagram + the `FailFast` / `KeepGoing` sequence diagrams
+ design decisions live in [`ARCHITECTURE.md`](ARCHITECTURE.md).

| Crate | Purpose |
|---|---|
| [`repolith-core`](crates/repolith-core) | Types, traits (`Action`, `Cache`), manifest parser, layered `Plan`. |
| [`repolith-cache`](crates/repolith-cache) | `SqliteCache` (rusqlite, bundled, WAL). |
| [`repolith-engine`](crates/repolith-engine) | Async `Orchestrator` with cancellation + semaphore. |
| [`repolith-actions`](crates/repolith-actions) | `GitClone` (feature `git`), `CargoInstall` (feature `cargo`). |
| [`repolith-cli`](crates/repolith-cli) | `repolith sync / status` — the binary you run. |

## Status

**v0.0.4 shipped.** 5 crates, 2 builtin actions, parallel layered execution
with cancellation, SQLite cache (WAL), URL-injection hardening,
process-group cancel propagation, node-id path-traversal validator. Full
test suite under `cargo test --workspace --all-features` (80+ tests at last
count). See [`CHANGELOG.md`](CHANGELOG.md) for the per-release breakdown
(three pre-public audit cycles between v0.0.1 and v0.0.4 closed every
must-fix item surfaced).

> **CI status.** GitHub Actions runs are temporarily paused while the
> upstream Actions billing is being sorted out — the workflow in
> [`.github/workflows/ci.yml`](.github/workflows/ci.yml) is the canonical gate
> (`cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
> `cargo test --workspace --all-features`, `cargo doc` with `RUSTDOCFLAGS=-D warnings`).
> Run it locally before opening a PR.

### Roadmap

- **M2** — federation `kind = "repolith"` (orchestrator-of-orchestrators),
  Neo4j cache backend, `docker` action.
- **M3** — watch mode (re-plan on file change), `template_apply` action
  driving `AttachedEntry::Outbound`.

## Security

See [`SECURITY.md`](SECURITY.md) for the threat model, the env-allowlist
policy, and the GitHub Security Advisories private-reporting form for
vulnerability disclosure.

## Contributing

PRs welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev setup,
local CI gates, and worked recipes for adding a new action or a new cache
backend.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
