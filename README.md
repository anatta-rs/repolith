# repolith

> Multi-repo orchestration for Rust ecosystems. Lightweight. Graph-aware.

This crate name is **reserved on crates.io**. The real implementation is
incubating in [`anatta-rs/anatta`](https://github.com/anatta-rs/anatta)
and will land here once the API stabilizes (target: 0.1.0).

## What repolith will be

A declarative, content-addressed orchestrator for Rust projects spread
across multiple sibling git repositories. Three core concepts:

- **node** — a managed git repo (services, CLIs, libraries)
- **action** — one step of work (cargo install, docker compose, deploy
  templates, project artifacts into a graph, ...)
- **attached** — a consumer repo that pulls templates from the
  orchestrator (e.g. shared `.claude/` or `.githooks/` files)

Pluggable cache backends (sqlite default, Neo4j optional for projects
that want a queryable causal trace of every build).

## What repolith will NOT be

The negative scope is **fixed** and will not evolve:

- Not a CI runner — no distributed execution, no remote workers
- Not a toolchain manager — `rustup` is fine
- Not a package manager — `cargo` is fine
- Not a process supervisor — `systemd` / `launchd` / `docker compose`
  are fine; repolith reads heartbeats, doesn't write them
- Not a monorepo tool — `cargo workspaces` already covers single-repo
  workspace publishing
- Not a hermetic build system — hermeticity is opt-in per action, not
  the default

## Architecture (target — v0.1)

GDD-validated layout for milestone [v0.0.1 — bootstrap M1](https://github.com/anatta-rs/repolith/milestone/1).
4 crates, 3 traits, layered execution with `FuturesUnordered` + `CancellationToken`.

```mermaid
graph TD
    subgraph CORE["crates/repolith-core (lib)"]
        T_ACT["trait Action<br/>execute → Result&lt;BuildOutput, BuildError&gt;<br/>+ CancellationToken in Ctx"]
        T_CACHE[trait Cache]
        T_SRC[trait Source]
        E_BUILD["BuildError<br/>· UpstreamUnreachable<br/>· CommandFailed{exit, stderr}<br/>· Io · Cancelled"]
        E_PLAN["PlanError<br/>· Cycle{path}<br/>· MissingDep{from, to}"]
        EM["ExecMode<br/>FailFast (cancel layer)<br/>KeepGoing (settle then halt)"]
        P_PLAN[Plan<br/>layers: Vec&lt;Vec&lt;ActionId&gt;&gt;<br/>reasons: HashMap&lt;_, ChangeReason&gt;]
        T_ORCH[Orchestrator<br/>builder · cache · manifest · registry<br/>max_parallelism · mode]
        T_TYPES[ActionId · Sha256 · BuildEvent · Ctx]
        T_MAN[Manifest types<br/>+ AttachedEntry+Direction]
    end

    subgraph CACHE["crates/repolith-cache-sqlite"]
        SQL[SqliteCache::open path]
    end
    subgraph ACT["crates/repolith-actions (feature-gated)"]
        F_GIT["GitClone<br/>(feature='git')"]
        F_CARGO["CargoInstall<br/>(feature='cargo')"]
    end
    subgraph CLI["crates/repolith-cli (bin)"]
        BIN[main.rs · clap]
        FLAGS["sync flags<br/>--explain · --dry-run<br/>-j N · -k/--keep-going"]
    end

    SQL -.impl.-> T_CACHE
    F_GIT -.impl.-> T_ACT
    F_CARGO -.impl.-> T_ACT
    BIN --> T_ORCH
    T_ORCH --> P_PLAN
    T_ORCH --> EM
    T_ACT --> E_BUILD
```

### Sequence — `repolith sync` with `ExecMode::FailFast`

Mid-layer cancellation via `FuturesUnordered` + shared `CancellationToken`
(NOT `tokio::join_all` — see [#6](https://github.com/anatta-rs/repolith/issues/6)).

```mermaid
sequenceDiagram
    participant CLI
    participant Orch as Orchestrator
    participant Pool as FuturesUnordered<br/>+ CancellationToken
    participant A1 as A1 (slow)
    participant B1 as B1 (fast, fails)
    participant C1 as C1 (slow)

    CLI->>Orch: execute_plan(plan, FailFast)
    Orch->>Pool: spawn(A1, B1, C1) with shared cancel
    par
        A1->>Pool: poll
    and
        B1->>Pool: poll
    and
        C1->>Pool: poll
    end
    B1-->>Pool: Err(CommandFailed) [first to settle]
    Pool-->>Orch: yield Err
    Orch->>Pool: cancel.cancel()
    Note over A1,C1: tokio::select on cancel<br/>→ Err(Cancelled) immediately
    A1-->>Pool: Err(Cancelled)
    C1-->>Pool: Err(Cancelled)
    Pool-->>Orch: drain remaining (3 events total)
    Orch-->>CLI: Err(LayerFailed{events}) — layer N+1 NEVER STARTS
```

## Status

`0.0.0` is a placeholder. Watch the GitHub repo for milestones.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
