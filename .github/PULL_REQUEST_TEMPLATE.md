<!--
Thanks for opening a PR against quiknode-labs/shredstream-proxy.

This repository uses a two-branch model:
  - `main-qn` (default) — QN's working branch. All day-to-day PRs target here.
  - `master`            — clean mirror of jito-labs/shredstream-proxy, sync only.

Day-to-day work targets `main-qn`. If the change is also useful upstream,
follow up after merge with a cherry-pick onto a branch off `master` and
open an upstream PR against `jito-labs/shredstream-proxy:master`.

See CONTRIBUTING.md for the full workflow.

PR TITLE: use a conventional-commit message (e.g. `feat: ...`, `fix: ...`,
`feat!: ...`). PRs are squash-merged and release-please reads the squash
commit (= this title) to compute the next version. A non-conventional title
ships no version bump. See "Versioning & releases" in CONTRIBUTING.md.
-->

## Is this change upstreamable?

- [ ] **Yes** — broadly useful (bug fix, generic feature, refactor). After
      this PR merges into `main-qn`, cherry-pick onto a branch off `master`
      and open a PR against `jito-labs/shredstream-proxy:master`.
- [ ] **No** — QuickNode-specific (CI runner, deploy paths, OCI bucket,
      fleet defaults, operational tooling). Use a `qn/*` branch name and
      do **not** open an upstream PR.

## Summary

<!-- 1–3 bullets describing what changed and why. -->

## Test plan

<!-- How was this verified? cargo test locally, deploy to staging,
     specific receiver_stats journal lines, etc. -->
