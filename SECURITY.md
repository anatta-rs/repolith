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

**Do not open a public GitHub issue for vulnerabilities.** Email
[the maintainers](https://github.com/anatta-rs/repolith/blob/main/Cargo.toml)
(use the `authors` field for the contact list) with:

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

### What's hardened in M1

- **Argument injection (CWE-88)** — `node.git` URLs are validated against
  a scheme allowlist (`https://`, `http://`, `ssh://`, `git@`, `file://`)
  and rejected if they start with `-` (which `git` would interpret as a
  flag). Crate names and feature names that start with `-` or contain
  `,` are rejected at manifest parse time.
- **Environment leak (CWE-200)** — the orchestrator's `Ctx::env` only
  carries an allowlisted subset of the parent process env (`PATH`,
  `HOME`, `USER`, `SHELL`, `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME`,
  `RUSTUP_TOOLCHAIN`, `RUST_LOG`, `RUST_BACKTRACE`, `TZ`, `LANG`,
  `LC_ALL`). Tokens like `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, etc.
  remain in the parent process and never enter `repolith`'s data flow,
  so they can't leak through a `tracing::debug!(?ctx)` call or a panic
  dump.
- **SQL injection** — every cache write goes through `rusqlite::params!`
  parameterized queries; no string concatenation reaches the SQLite
  driver.
- **Stable cancellation** — every spawned subprocess races against a
  shared `CancellationToken` so `Ctrl-C` (and SIGTERM in M2)
  short-circuits the orchestrator without leaving zombie git/cargo
  processes (PR3).

### Known M1 limitations (deferred to M2)

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
- **Subprocess argument injection beyond URL/crate/features** — the
  `node.git` allowlist covers the URL itself, but the orchestrator
  doesn't probe deeper into git's URL grammar (e.g. `core.sshCommand`
  configured at clone time via a hostile config). Use `https://` URLs
  with vetted hosts.
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
