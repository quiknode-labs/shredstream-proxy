# Contributing

This repository is a fork of [`jito-labs/shredstream-proxy`](https://github.com/jito-labs/shredstream-proxy)
maintained by QuickNode. Changes land here for two reasons:

1. **Upstreamable improvements** — bug fixes, new features, or refactors
   that are broadly useful and should eventually flow to
   `jito-labs/shredstream-proxy`.
2. **Fork-specific overrides** — CI runners, deploy plumbing, operational
   tooling, and other things tied to QuickNode's infrastructure that
   don't belong upstream.

To keep these straight and avoid accidentally proposing fork-specific
changes upstream, we use a branch-naming convention and a PR template
checkbox.

## Branch naming

| Prefix | Use for | Upstream-bound? |
|---|---|---|
| `qn/*` | Fork-specific changes — CI workflows, build pipeline shims, infra integration, fleet defaults | **No** — never open an upstream PR from these |
| `feature/*` or unprefixed | Generic improvements — bug fixes, new features, refactors | Yes, optionally |

Examples:
- `qn/ci-make-runnable-on-fork` — CI runner switch from Jito's self-hosted
  pool to GitHub-hosted runners. Fork-specific.
- `qn/oci-binary-path-update` — change OCI bucket layout the deploy
  pipeline reads from. Fork-specific.
- `add-listen-port-device-metric-labels` (or `feature/...`) — new InfluxDB
  tags on `receiver_stats`. Useful to anyone running the proxy → eligible
  to upstream after merging here.

If unsure, pick `qn/*`. It's safer to under-promote a generic change than
to accidentally surface a fork-specific one upstream.

## Opening a PR

Always specify the target repo explicitly when using `gh pr create`. The
fork's default for `gh` is the upstream repo, which can lead to mis-routed
PRs:

```bash
# Fork-only (QN deploy depends on this PR):
gh pr create --repo quiknode-labs/shredstream-proxy --base master \
  --head qn/<branch>

# Upstream contribution (after the fork PR has merged):
gh pr create --repo jito-labs/shredstream-proxy --base master \
  --head <unprefixed-branch>
```

The PR template at `.github/PULL_REQUEST_TEMPLATE.md` includes a checkbox
to confirm the upstream/fork status at PR creation time — please tick the
right box.

## Syncing with upstream

To pull new commits from `jito-labs/shredstream-proxy:master`:

```bash
gh repo sync quiknode-labs/shredstream-proxy --source jito-labs/shredstream-proxy
```

Or the GitHub UI's "Sync fork" button on the repo page. This only ever
pulls from upstream — it never pushes our `qn/*` changes anywhere.

## Local development

Standard Rust toolchain workflow (see `rust-toolchain.toml` for the pinned
version). Build deps: `protoc` (Protobuf compiler) and the submodule under
`jito_protos/protos/` initialized:

```bash
git submodule update --init --recursive
cargo build --release
cargo test --all-features
```
