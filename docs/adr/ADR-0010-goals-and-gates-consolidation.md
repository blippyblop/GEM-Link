# ADR-0010 — Goals, score and gates consolidated into ROADMAP as the single source of truth

- **Status:** Accepted
- **Date:** 2026-10-01
- **Deciders:** solo maintainer

## Context

The project's goals had drifted apart across the documents that state them, and one of
them — the score — was stated three different ways:

- `ROADMAP.md` north star: "minimum **measured glass-to-glass latency** as the score".
- `ROADMAP.md` DoD #3: "bench-measured **glass-to-glass ≤ Steam Link/VR Link** on
  identical hardware and link" — with **no measurement method named**, and unmeasurable
  without a Frame.
- The code itself (`x-bench`): the real, already-implemented metric —
  `MANDATORY_DEADLINE_MS = 1000/90`, `OPTIMAL_DEADLINE_MS = 1000/120`,
  `delivery.missed_mandatory_pct`, gated in CI — which `ROADMAP.md` never mentioned.

Concretely, the drift was:
- **Numbers disagreed.** Refresh was 90–120 Hz in ROADMAP, 72–144 Hz in README,
  "72/80/90/120 (144 exp.)" in PRESETS, "90" in COMPAT. COMPAT listed the Frame as
  AV1 while ADR-0008 says the kernel decodes HEVC/H.264 only.
- **Status disagreed with reality.** M1's gate was met on 2026-10-01 but shown
  unchecked; `x-protocol` existed but was unchecked in M0; `x-dda`/`x-idd` were built
  and verified but unchecked in M2.
- **Items had no home.** "Encrypted transport default-on" is a DoD item and a published
  `SECURITY.md` promise, but *wiring* the existing crypto had no milestone. Client-side
  de-foveation — without which the 300 Mbps envelope is not real — had none either.
- **A link dangled.** `docs/BENCH.md` was referenced by README, CONTRIBUTING and the
  README skeleton, and did not exist.
- **The competitive bar** ("as good as the shipping stack or the project failed") had no
  durable home.

"Dates are guidance, **gates are law**" is only true if the gates are unambiguous and
their status is honest. They were not.

## Options considered

1. **Leave the documents as they are and clarify in conversation.** — Rejected: the next
   session re-derives the position, which is exactly the cost this project has already
   paid twice.
2. **Rewrite every doc independently, restating the targets in each.** — Rejected: N
   copies of a number is N chances to disagree; this is how the drift happened.
3. **One canonical statement of goals/score/gates, everything else links to it.** —
   Chosen.

## Decision

1. **`ROADMAP.md` is the single source of truth** for goals, the score, and gates. Where
   a number appears in two places, ROADMAP wins; other docs **link** rather than restate.
2. **The score of record is the bench metric**, stated as a tail:
   - Mandatory gate: **0 % of frames miss 1000/90 ms = 11.11 ms** (`missed_mandatory_pct`).
   - Published target: **1000/120 ms = 8.33 ms** (`within_optimal_pct`) — tracked, not enforced.
   - Secondary: absolute latency p50/p95/p99 minimized.
   - The **glass-to-glass field bar** (≤ Steam Link/VR Link, identical hardware and
     link) remains in the DoD but is marked **⛔ hardware-gated**, with its method named
     in `docs/BENCH.md` — not invented at comparison time.
3. **Two new documents give the homeless things a home.**
   - `docs/BENCH.md` — what is measured, the metrics schema, scenarios, gate rules, and
     an explicit "does not measure" section (glass-to-glass, real decode, client
     coverage). Fixes the dangling link.
   - `docs/BAR.md` — the competitive bar, the comparison method, the refusal list, and
     the record that Virtual Desktop is a **single-developer** product (so the bar is
     reachable by an individual and there is no headcount excuse).
4. **Canonical device numbers:** Steam Frame 2160×2160/eye LCD, **panel 72–144 Hz**,
   **gating envelope 90–120 Hz**; decode **HEVC/H.264** (no AV1/10-bit, ADR-0008);
   **render resolution is the client's call** (never pinned to a constant).
5. **Milestone status corrected to reality**, and the homeless items assigned:
   encryption **wiring** → M1 (with its hot-path design trap recorded); client-side
   de-foveation + the Frame client binary + V4L2 iris decode → M3; `refresh=72 Hz`
   investigation → M1.
6. **Known coverage gaps are stated in ROADMAP**, not buried: CI builds neither
   `client_core` nor the harnesses, and no glass-to-glass measurement exists.

## Consequences

- **Easier:** a fresh session reads `ROADMAP.md` and knows the target, the score and the
  honest status; the score is checkable in-repo (`bench run` / `bench gate`) and links to
  a method for the parts that need hardware.
- **Harder / accepted:** the field bar stays a **gated claim**, not a measured one, until
  hardware exists — stated wherever it appears. The project's headline promise
  (encryption always on) is recorded as a promise not yet wired, which is uncomfortable
  and correct.
- **Revisit trigger:** the glass-to-glass harness lands (then DoD #3's field bar moves
  from ⛔ to measurable and `docs/BAR.md` gains real numbers); or the Frame firmware
  unlocks AV1/10-bit (ADR-0008 revisit, which regenerates the goldens and the canonical
  codec line).
