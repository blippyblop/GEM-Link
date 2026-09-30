# ADR-0001 — License: MIT (unchanged from upstream), "do what you want"

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** solo maintainer (blippyblop)

## Context
The fork derives from ALVR, which is MIT (© polygraphene 2018–2019, © alvr-org 2020–2024). Charter goal: free, open, non-commercial in *ethos*. Options considered: keep MIT, sublicense GPL-3.0, or add NC terms.

## Constraints (fact, not opinion)
- NC licenses (CC-BY-NC, PolyForm-NC) are **legally unavailable**: MIT's grant is irrevocable and permits commercial use; it cannot be narrowed after the fact.
- MIT→GPL sublicensing is possible (one-way), but adds license-mixing bookkeeping and a copyleft mandate the project doesn't want.

## Decision
**Keep MIT, unchanged.** The fork ships under the same MIT text as upstream (preserve the existing copyright lines; append the fork's own line). "Do what you want, attribution included."

## Consequences
- **Pros:** zero friction — no mixed-license bookkeeping, no SPDX dual entries, maximal downstream freedom, perfect upstream compatibility, simplest possible CONTRIBUTING/NOTICE story. Matches the project's "bruh, just use it" energy.
- **Cons (accepted):** anyone may build commercial products on it. The charter's non-commercial *ethos* is enforced by policy (our builds are free, our name is trademarked) — not by license. That is inherent to "do what you want."
- WiVRn (GPL) code exchange is *off* the table — ideas/APIs still fine. Accepted trade.
