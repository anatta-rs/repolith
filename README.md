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
🟢 = additions vs the empty placeholder (everything is net-new in M1).

```mermaid
graph TD
    subgraph CORE["crates/repolith-core (lib)"]
        T_ACT["trait Action<br/>execute → Result&lt;BuildOutput, BuildError&gt;<br/>+ CancellationToken in Ctx"]:::add
        T_CACHE[trait Cache]:::add
        T_SRC[trait Source]:::add
        E_BUILD["<b>BuildError</b><br/>· UpstreamUnreachable<br/>· CommandFailed exit_code, stderr<br/>· IoError<br/>· Cancelled"]:::add
        E_PLAN["<b>PlanError</b><br/>· Cycle path<br/>· MissingDep id"]:::add
        EM["<b>ExecMode</b><br/>FailFast (cancel layer)<br/>KeepGoing (settle then halt)"]:::add
        P_PLAN[Plan layers + reasons<br/>frozen DAG]:::add
        T_ORCH[Orchestrator<br/>builder cache manifest registry<br/>max_parallelism, mode]:::add
        T_TYPES[ActionId, Sha256, BuildEvent, Ctx]:::add
        T_MAN[Manifest types<br/>+ AttachedEntry+Direction]:::add
    end

    subgraph CACHE["crates/repolith-cache-sqlite"]
        SQL[SqliteCache::open path]:::add
    end
    subgraph ACT["crates/repolith-actions (feature-gated)"]
        F_GIT[GitClone]:::add
        F_CARGO[CargoInstall]:::add
    end
    subgraph CLI["crates/repolith-cli (bin)"]
        BIN[main.rs · clap]:::add
        FLAGS["sync flags:<br/>--explain · --dry-run<br/>-j N · -k/--keep-going"]:::add
    end
    TESTS["tests/<br/>manifest_parse + sync_smoke<br/>+ plan_layers + parallel_exec<br/>+ failfast_cancels + keepgoing_settles"]:::add

    SQL -.impl.-> T_CACHE
    F_GIT -.impl.-> T_ACT
    F_CARGO -.impl.-> T_ACT
    BIN --> T_ORCH
    T_ORCH --> P_PLAN
    T_ORCH --> EM
    T_ACT --> E_BUILD

    classDef add fill:#90EE90,stroke:#228B22,color:#000
```

### Sequence — `ExecMode::FailFast` (mid-layer cancel)

Cancellation via `FuturesUnordered` + shared `CancellationToken` —
NOT `tokio::join_all` (see [#6](https://github.com/anatta-rs/repolith/issues/6)
implementation note).

```mermaid
sequenceDiagram
    participant CLI
    participant Orch as Orchestrator
    participant Tok as CancellationToken
    participant L1a as Action A1<br/>(slow)
    participant L1b as Action B1<br/>(fast, fails)
    participant L1c as Action C1<br/>(slow)
    participant L2 as Layer 2

    CLI->>Orch: execute_plan(plan, FailFast)
    Note over Orch: spawn layer 1 concurrent
    par
        Orch-)L1a: execute(ctx, token)
    and
        Orch-)L1b: execute(ctx, token)
    and
        Orch-)L1c: execute(ctx, token)
    end
    L1b-->>Orch: Err(CommandFailed)
    Orch->>Tok: cancel
    Tok-->>L1a: cancel signal
    Tok-->>L1c: cancel signal
    L1a-->>Orch: Err(Cancelled)
    L1c-->>Orch: Err(Cancelled)
    Note over Orch,L2: layer 2 NEVER STARTS
    Orch-->>CLI: Err(LayerFailed { events: [..] })
```

### Sequence — `ExecMode::KeepGoing` (settle then halt)

Each layer runs to completion regardless of failures ; the orchestrator
halts at the layer boundary if anything failed in the current layer.
Useful for surfacing all failures of a layer in one run.

```mermaid
sequenceDiagram
    participant CLI
    participant Orch as Orchestrator
    participant L1a as Action A1
    participant L1b as Action B1<br/>(fails)
    participant L1c as Action C1
    participant L2 as Layer 2

    CLI->>Orch: execute_plan(plan, KeepGoing)
    par
        Orch-)L1a: execute()
    and
        Orch-)L1b: execute()
    and
        Orch-)L1c: execute()
    end
    L1b-->>Orch: Err(CommandFailed)
    L1a-->>Orch: Ok(BuildOutput)
    L1c-->>Orch: Ok(BuildOutput)
    Note over Orch: layer settles<br/>1 failure, 2 ok
    Note over Orch,L2: any err → halt before next layer
    Orch-->>CLI: Err(LayerFailed { events: [Ok, Err, Ok] })
```

## Status

`0.0.0` is a placeholder. Watch the GitHub repo for milestones.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
