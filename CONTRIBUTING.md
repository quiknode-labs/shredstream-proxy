# Contributing

This repository is QuickNode's fork of
[`jito-labs/shredstream-proxy`](https://github.com/jito-labs/shredstream-proxy).
We use a two-branch model to keep fork-specific overrides separated from
upstream-eligible work, so accidental upstream contribution of QN-only code
is unlikely.

## The two branches

| Branch | Role | Where PRs target | What lives here |
|---|---|---|---|
| **`main-qn`** | Default branch on the fork. QN's working line. | All day-to-day PRs. | Everything in `master` + QN-specific patches (CI, deploy plumbing, fleet defaults). |
| **`master`** | Clean mirror of `jito-labs/shredstream-proxy:master`. | Only branches destined for upstream contribution. | Exactly what's upstream, nothing more. Refreshed via `gh repo sync`. |

The deploy pipeline (`role_chain_build`) builds release tags. Tags are
created from `main-qn`, so the resulting binary always carries the QN
patches.

## What's currently QN-specific

As of this writing, the diff between `main-qn` and `master` is:

| Change | What it does |
|---|---|
| `proxy/src/forwarder.rs` + `proxy/src/main.rs` (metric label patch) | Adds `listen_port` and `device` tags to the `shredstream_proxy-receiver_stats` metric so multicast (`device=doublezero1`) and unicast (`device=unicast`) traffic from the same source IP can be attributed separately. Originally needed to measure DoubleZero's marginal contribution to first-arrival shred throughput. Eligible for upstream contribution. |
| `.github/workflows/*.yml`, `.github/actions/setup-rust/action.yaml` | Switches CI from Jito's private self-hosted runners (`ubuntu-22.04-16c-64g-public`) to stock `ubuntu-22.04`. Replaces Docker Hub publish steps with direct `cargo build --release`. Uses `actions-rust-lang/setup-rust-toolchain` for the toolchain (reads `rust-toolchain.toml`). QN-specific — would not be appropriate upstream. |
| `.github/PULL_REQUEST_TEMPLATE.md`, `CONTRIBUTING.md` | Fork-management policy (this document). QN-specific. |

When evaluating whether a new change is upstreamable, the metric label
patch is a useful reference for what "broadly useful" looks like.

## Branch protection

The fork has GitHub Rulesets configured to enforce the two-branch model:

- **`master-mirror-lock`** ruleset on `master`: blocks direct pushes,
  force-pushes, deletions, and non-fast-forward updates. Only repo
  admins running `gh repo sync` (which uses the GitHub API, not git
  push) can advance `master`. This guarantees `master` never diverges
  from upstream.
- **`main-qn-pr-review`** ruleset on `main-qn`: requires a pull request
  with at least one approval before merge. Blocks force-pushes and
  deletions. CI status checks (`test`, `build`) are required to pass.

If you try `git push origin master` directly, the push will be rejected
by the ruleset. That's intended — go through `gh repo sync` for upstream
updates, or open a PR against `main-qn` for QN-specific work.

The active CI workflows on every PR to `main-qn`:

| Workflow | What it runs |
|---|---|
| `test` | `cargo test --all-features --locked` |
| `build` | `cargo clippy --all-features --all-targets --tests -- -D warnings` and `cargo build --release --locked --bin jito-shredstream-proxy` |

Both must pass before merge. The `release` workflow only runs on `v*` tag pushes — it produces a GitHub Release asset.

## Branch naming

| Prefix | Use for | Branch off | PR target |
|---|---|---|---|
| `qn/*` | Fork-specific changes — CI workflows, deploy paths, infra integration, operational tooling | `main-qn` | `main-qn` |
| `feature/*` or unprefixed | Generic improvements — bug fixes, generic features, refactors | `main-qn` (initially) | `main-qn` (initially); `master` later if upstreamed |

If you're unsure whether a change is upstreamable, branch off `main-qn`
and target `main-qn`. The decision of whether to also send it upstream
can be made after review.

## Day-to-day workflow (fork-only change)

```bash
git checkout main-qn
git pull
git checkout -b qn/my-fork-only-change

# ... make changes, commit ...

git push -u origin qn/my-fork-only-change
gh pr create \
  --repo quiknode-labs/shredstream-proxy \
  --base main-qn \
  --head qn/my-fork-only-change
```

Always include `--repo` and `--base` explicitly. The default for `gh pr
create` is the upstream repo's default branch, which is wrong here.

## Workflow for an upstreamable change

Do the work against `main-qn` first so the fleet can deploy it, then
mirror it upstream:

```bash
# 1. Land it in our fork
git checkout main-qn
git pull
git checkout -b feature/my-improvement
# ... commits ...
git push -u origin feature/my-improvement
gh pr create \
  --repo quiknode-labs/shredstream-proxy \
  --base main-qn \
  --head feature/my-improvement
```

After that PR merges into `main-qn`:

```bash
# 2. Cherry-pick the same change(s) onto a branch off the clean master
git checkout master
git pull
git checkout -b upstream/my-improvement
git cherry-pick <commit-sha-from-main-qn>
git push -u origin upstream/my-improvement

# 3. Open the upstream PR
gh pr create \
  --repo jito-labs/shredstream-proxy \
  --base master \
  --head McSim85:upstream/my-improvement  # or your GitHub user
```

After upstream merges your PR, the next `gh repo sync` pulls it back into
our `master`, and a routine merge of `master` into `main-qn` removes the
locally-applied version cleanly.

## Syncing `master` with upstream

Periodically (or when there's something specific you want from upstream):

```bash
gh repo sync quiknode-labs/shredstream-proxy \
  --source jito-labs/shredstream-proxy \
  --branch master
```

This is one-way: it only pulls from upstream, never pushes our changes
back. After sync, merge `master` into `main-qn` to bring upstream changes
into our working line:

```bash
git checkout main-qn
git pull
git merge --no-ff origin/master
# resolve conflicts on overridden files (workflows, etc.) — keep our versions
git push
```

`git rerere` is helpful here if you keep getting the same conflicts.

## Opening a PR — quick reference

```bash
# Fork-only (most common case):
gh pr create --repo quiknode-labs/shredstream-proxy --base main-qn --head qn/<branch>

# Upstreamable, first land in fork:
gh pr create --repo quiknode-labs/shredstream-proxy --base main-qn --head feature/<branch>

# Upstreamable, mirror to upstream after fork-side merge:
gh pr create --repo jito-labs/shredstream-proxy --base master --head <user>:upstream/<branch>
```

The PR template at `.github/PULL_REQUEST_TEMPLATE.md` includes a checkbox
that asks whether a change is upstreamable. Tick the right box so reviewers
know whether to expect a follow-up upstream PR.

## Local development

Standard Rust workflow. Pinned toolchain in `rust-toolchain.toml`,
`protoc` required, and the submodule under `jito_protos/protos/` needs
to be initialized:

```bash
git submodule update --init --recursive
cargo build --release
cargo test --all-features
```
