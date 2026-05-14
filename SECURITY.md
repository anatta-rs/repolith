# Security policy

## Supported versions

`repolith` is pre-1.0. Only the latest tagged release receives security
fixes. Use `cargo install --locked --git https://github.com/anatta-rs/repolith`
to track `main` and pull fixes as they land.

| Version  | Supported          |
|----------|--------------------|
| `0.0.x`  | :white_check_mark: |
| `< 0.0.1`| :x:                |

## Reporting a vulnerability

**Do not open a public GitHub issue for vulnerabilities.** Use GitHub's
private vulnerability reporting:
[`anatta-rs/repolith/security/advisories/new`](https://github.com/anatta-rs/repolith/security/advisories/new).
Include:

- The `repolith --version` you reproduced against.
- The smallest `repolith.toml` that triggers the issue.
- The exact command and the observed vs. expected behavior.
- Any disclosure timeline you'd like respected.

Acknowledged within 5 business days. A fix or mitigation lands on `main`
within 30 days; we'll coordinate a CVE assignment if the impact warrants
one.

## Threat model — what `repolith` defends against (and what it doesn't)

### Trust boundary

The **manifest is the trust boundary**. The operator running `repolith
sync` is assumed to have vetted `repolith.toml` — its URLs, paths, crate
names, install destinations. `repolith` performs argument-injection
hardening (see below) so a *typo* or a *moderately hostile* contributor
can't trivially escape the subprocess argv, but it does **not**
sandbox what the manifest can name.

If your workflow involves merging `repolith.toml` changes from
contributors you don't fully trust (e.g. via PR), treat manifest review
as you would treat reviewing a new shell script that runs `git`,
`cargo`, and writes to disk.

### What's hardened today

- **Argument injection (CWE-88)** — `node.git` URLs are validated against
  a scheme allowlist (`https://`, `http://`, `ssh://`, `git@`, `file://`),
  rejected if the URL itself, its userinfo, host, or any path segment
  starts with `-` (which `git` / `ssh` would interpret as a flag), and
  rejected if the URL contains any control character. Subprocess invocations
  insert a literal `--` argv separator before the user-supplied URL as
  defense in depth. Crate names and feature names that start with `-` or
  contain `,` are rejected at manifest parse time.
- **Environment leak (CWE-200)** — the orchestrator's `Ctx::env` only
  carries an allowlisted subset of the parent process env. The list
  itself lives at `ENV_ALLOWLIST` in
  [`crates/repolith-cli/src/main.rs`](crates/repolith-cli/src/main.rs)
  — refer to source for the current set; enumerating it here would
  drift on every change. Tokens like `GITHUB_TOKEN`,
  `AWS_SECRET_ACCESS_KEY`, etc. remain in the parent process and never
  enter `repolith`'s data flow, so they can't leak through a
  `tracing::debug!(?ctx)` call or a panic dump. SDK consumers of
  `repolith-engine` must pass an explicit `Ctx::env` to `.base_ctx(...)`
  — the engine's default builder ships an **empty** env map for the
  same reason.
- **SQL injection** — every cache write goes through `rusqlite::params!`
  parameterized queries; no string concatenation reaches the SQLite
  driver.
- **Stable cancellation** — every spawned subprocess races against a
  shared `CancellationToken` so both `Ctrl-C` (SIGINT) and SIGTERM
  short-circuit the orchestrator without leaving zombie git/cargo
  processes. On Unix, children are placed in their own process group
  and the group is signalled (SIGTERM → grace → SIGKILL) so cargo's
  `rustc`/linker grandchildren are reaped, not just the direct child.

### Known limitations (deferred to M2)

These are real risks under a hostile-manifest threat model, *not*
exploitable by a benign typo. They're called out so operators can make
informed deployment decisions.

- **Path traversal (CWE-22)** — `node.path` and
  `[[node.action]].install_to` accept any absolute or relative path. A
  hostile manifest can therefore set `install_to = "/usr/local"` and
  cause `cargo install --root /usr/local` to overwrite system binaries
  on next sync (assuming the operator has write access there). The
  workaround for now is to vet `install_to` and `path` in code review.
  M2 will add a configurable sandbox root (defaulting to `~/.repolith`
  for installs and `./` for clones) with an explicit opt-out for
  trusted manifests.
- **Subprocess argument injection beyond URL grammar** — the URL,
  userinfo, host, and path-segment leading-dash checks cover everything
  that reaches argv as a positional, but the orchestrator doesn't probe
  deeper into git's *runtime* config surface (e.g. `core.sshCommand`
  injected via a hostile per-repo `.git/config` after the first fetch,
  or via a `~/.gitconfig` an attacker controls). Use `https://` URLs
  with vetted hosts when running against untrusted networks.
- **Resource exhaustion** — a manifest declaring tens of thousands of
  nodes will allocate proportionally; there's no per-manifest cap. The
  semaphore caps *concurrent* in-flight subprocesses, not total work.
- **Git submodules** — `git clone` follows submodules by default if the
  upstream `.gitmodules` declares any. M1 does not pin submodule
  revisions or audit submodule URLs against the same allowlist as the
  parent.

If any of these limitations affects your deployment, please open a
non-security GitHub issue tagged `area:security` describing your use
case so we can prioritize the M2 sandbox feature accordingly.

## Hardening checklist for operators

When running `repolith sync` against a manifest you didn't write:

- [ ] Run as an unprivileged user (no `sudo`).
- [ ] Set `install_to` to a directory the user owns (`~/.local/bin`,
      `~/.repolith/bin`).
- [ ] Avoid running with `RUST_LOG=trace` in production — logs may
      include manifest paths that contain sensitive context.
- [ ] If the manifest references private repos, configure SSH keys
      with the minimum scope required (read-only).
- [ ] In CI, use a fresh ephemeral runner so any half-completed action
      can't poison subsequent runs.
