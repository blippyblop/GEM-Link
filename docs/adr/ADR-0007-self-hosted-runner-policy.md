# ADR-0007 — Self-hosted runner security policy

Status: Accepted (2026-09-30)
Deciders: maintainer

## Context

The windows GPU delivery gates need a self-hosted runner (the reference
RTX 5080 box). On a public repository, the default `pull_request` trigger
lets fork PRs execute workflows on self-hosted runners — and `cargo build`
runs arbitrary `build.rs` scripts, which is arbitrary code execution on the
maintainer's personal machine. Secrets are not exposed to fork PRs, but the
machine itself is the asset.

## Decision

1. **No `pull_request` trigger, ever, while the self-hosted runner exists.**
   Fork PRs never execute workflows on GemLink infra; CI for PR code happens
   after a maintainer merges or via explicit dispatch by a write-access user.
2. **Triggers limited to `push: [master]` + `workflow_dispatch`** — both
   require repository write access.
3. **Repository guard on every job**
   (`if: github.repository == 'blippyblop/GEM-Link'`): a fork's own copy of
   the workflow can never target our runner group even indirectly.
4. **Runner group restricted to this repository** (repo settings), and fork
   PR workflows require maintainer approval at the repository level as
   defense in depth.
5. The runner runs **interactively in the user's session** for now; the
   service conversion (when approved) should use a **dedicated low-privilege
   local account**, not SYSTEM and not the daily user. CI never needs the
   desktop (feeder mode), so a low-privilege account is sufficient for gates.

## Consequences

- External contributors get CI after merge (or a maintainer-run dispatch);
  acceptable at the current project stage, revisited if contributor volume
  grows (then: ephemeral runner or hosted GPU CI for PRs).
- The reference box remains a shared machine; the 0%-missed mandatory gate
  is the arbiter of measurement validity during shared use.
