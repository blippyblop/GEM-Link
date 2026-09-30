# ADR-0002 — Fork base, rename, and crate discipline

- **Status:** Accepted
- **Date:** 2026-09-30

## Context
Upstream ALVR at analysis time (2026-09): HEAD `9f118394`, ~445 commits past tag `v20.14.1` (2025-07-15), experiments lineage unshipped, declining cadence, bus factor ≈2–3. GemLink's roadmap requires identity-level features upstream cannot absorb.

## Decision
1. **Base:** fork at upstream HEAD `9f11839431f95f0df6764790d9a16f8e308a0d79` (includes 2026 foveation/graphics work), pinned and tagged `base/upstream-9f118394-20260929`. Rebase discipline: monthly upstream review; shared-crate patches tracked in `PATCHES.md`.
2. **Rename:** new product name `<NAME>` applied day one across crates, binaries, registry keys, and UI. Reason: community clarity + ownable trademark. MIT does not require it; etiquette and capture-protection do.
3. **Crate discipline:** all roadmap work in new crates `x-dda`, `x-idd`, `x-runtime`, `x-codec-*`, `x-policy`, `x-bench`. Shared crates (`sockets`, `packets`, `session`, `graphics`, `server_core`, `client_*`, `dashboard`) accept only minimal patches, each logged in `PATCHES.md` (name + one-line reason). This is the mechanism that keeps future rebases cheap and upstream contributions possible.
4. **Do not build** `server_openvr` for our targets (SteamVR-dependent); it remains in-tree for reference and upstream merges.

## Consequences
- Rebase effort stays proportional to `PATCHES.md` length — keep it short.
- Divergence trigger: if shared-crate changes exceed ~30% of their LOC, revisit extraction into a standalone repo.

---

> **Amendment 2026-09-30:** rename scope is **identity-first** (see ADR-0003 §Decision 5): product identity renames day one; `alvr_*` Cargo package names and the `alvr/` directory stay until the name is final. Crate rename = one-shot mechanical pass, recorded in PATCHES.md.
