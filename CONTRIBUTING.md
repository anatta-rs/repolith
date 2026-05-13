# Contributing to repolith

Thanks for considering a contribution. This document covers the dev
workflow, the conventions we hold the workspace to, and the two
extension recipes — adding a new action and adding a new cache
backend.

## Dev setup

```bash
# Stable Rust (edition 2024 → needs 1.85 or later)
rustup update stable

git clone https://github.com/anatta-rs/repolith && cd repolith
cargo build --workspace --all-features
cargo test  --workspace --all-features
```

Optional but useful:

```bash
rustup component add rustfmt clippy
```

## Running the CI gates locally

CI runs four gates on every PR — they all need to pass before merge.
Run the same checks locally before pushing:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test  --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Convenience: `cargo fmt --all` (without `--check`) auto-fixes
formatting; `cargo clippy --fix` auto-applies the safe Clippy
suggestions.

## Conventions

- **Lints in workspace `Cargo.toml`** apply to every crate
  (`unsafe_code = deny`, `missing_docs = warn`, `clippy::pedantic +
  clippy::all = warn`). Don't `#[allow(...)]` ad-hoc in source —
  fix the root cause or escalate the lint to `allow` workspace-wide
  with a one-line comment explaining why.
- **Errors are typed via `thiserror`** in library crates,
  `anyhow::Result` in the CLI binary only.
- **Async traits** use `async-trait` until native `dyn AsyncFn`
  stabilizes.
- **Subprocess actions** must race their `tokio::process::Command`
  against `ctx.cancel` — see
  `crates/repolith-actions/src/util.rs::run_with_cancel`. Any new
  action that shells out goes through that helper.
- **Tests live next to the code they exercise.** Unit tests inline
  with `#[cfg(test)] mod tests`; integration tests in
  `crates/<crate>/tests/`. Fixtures land under
  `tests/fixtures/<scenario>/`.
- **Docs.** Every public item needs a `///` doc; every module needs
  a `//!` intro. Cross-crate links use plain backticks
  (` ` `Plan` ` `) — rustdoc intra-doc resolution doesn't follow
  paths across crate boundaries reliably.

## Adding a new action

The pattern lives in `crates/repolith-actions/src/git_clone.rs`. To
add, say, a `DockerCompose` action:

1. **Decide the cargo feature.** Existing actions are gated behind
   features (`git`, `cargo`) so consumers only pull the deps they
   actually use. Add `docker = []` to
   `crates/repolith-actions/Cargo.toml` and gate the new module:

   ```rust
   // crates/repolith-actions/src/lib.rs
   #[cfg(feature = "docker")]
   pub mod docker_compose;
   ```

2. **Define the action struct.** Hold the user-facing config (paths,
   URLs, options) plus `id` and `deps`:

   ```rust
   pub struct DockerCompose {
       pub id: ActionId,
       pub compose_file: PathBuf,
       pub deps: Vec<ActionId>,
   }
   ```

3. **Implement the trait.**

   ```rust
   #[async_trait]
   impl Action for DockerCompose {
       fn id(&self) -> ActionId { self.id.clone() }
       fn deps(&self) -> Vec<ActionId> { self.deps.clone() }

       async fn input_hash(&self, ctx: &Ctx) -> Result<Sha256, BuildError> {
           // hash whatever determines "the inputs are the same":
           // file contents, env vars, tool version, ...
       }

       async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError> {
           let mut cmd = Command::new("docker");
           cmd.args(["compose", "-f", self.compose_file.to_str().unwrap(), "up", "-d"]);
           let out = run_with_cancel(cmd, &ctx.cancel).await?;
           check_status(&out)?;
           // build BuildOutput from observable side-effect (file hash, container id, ...)
       }
   }
   ```

4. **Wire it into the manifest factory.** In
   `crates/repolith-cli/src/factory.rs`, add a variant to
   `ActionEntry` (in `repolith-core`) and a match arm that returns
   `Box::new(DockerCompose { … })`.

5. **Write the integration test.** Mirror
   `crates/repolith-actions/tests/git_clone.rs` — set up a fixture
   with `tempfile::tempdir()`, exercise both the cold path and the
   re-run path, assert against the file system / process state.

6. **Update `repolith.toml.example`** with a node demonstrating the
   new action so docs and code stay in sync (the
   `test_example_fixture_parses` test will catch any drift).

## Adding a new cache backend

The trait lives in `crates/repolith-core/src/cache.rs`:

```rust
#[async_trait]
pub trait Cache: Send + Sync {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent>;
    async fn record(&mut self, event: BuildEvent) -> Result<()>;
}
```

To add a Redis backend:

1. **Pick the crate it lives in.** Either extend `repolith-cache`
   with a feature-gated module (`features = ["redis"]`) or create a
   sibling `repolith-cache-redis` crate. Pick by dep weight —
   anything that pulls a TLS stack or a runtime is best isolated.

2. **Implement `Cache`.** Use `tokio::task::spawn_blocking` if your
   driver is sync (we do this in `SqliteCache` because `rusqlite` is
   `!Send`).

3. **Persist `BuildError` losslessly.** `BuildEvent::Failed.error` is
   a typed enum — serialize it with `serde_json` (see how
   `SqliteCache` stores `error_json`) so re-plan logic can reason
   about *why* the prior run failed.

4. **Test against the same suite as `SqliteCache`.** Copy
   `crates/repolith-cache/tests/sqlite_cache.rs` and re-target your
   constructor. The 5 scenarios (record + retrieve, missing returns
   None, REPLACE on duplicate, failed roundtrip, parent-dir
   creation) cover the contract.

5. **Wire it into the CLI** — add a `--cache-backend` flag in
   `repolith-cli/src/main.rs` and dispatch in `build_orchestrator`.

## Pull request flow

1. Branch from `main`: `git checkout -b feat/<short-slug>`.
2. Write the change. Keep PRs focused — one concern per PR makes
   review fast.
3. Run the four CI gates locally before pushing.
4. Open the PR. The body should answer: **what changed**, **why**,
   **how to verify**. Reference the issue with `Closes #N` if there
   is one.
5. CI must be green to merge.

## Reporting bugs / requesting features

Open an issue with:
- `repolith --version`
- The smallest `repolith.toml` that reproduces the bug
- The exact command and the actual vs. expected output
- For panics, attach the backtrace (`RUST_BACKTRACE=1 repolith ...`)

For feature requests, describe the use case, not the proposed API —
that lets us scope correctly.
