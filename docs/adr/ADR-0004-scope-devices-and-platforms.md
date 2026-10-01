# ADR-0004 — Scope: device tiers and platform stance

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** solo maintainer (blippyblop)

## Context

ADR-0003 made the Steam Frame the primary, release-gating device. The maintainer has
clarified broader scope: the Frame is the *goal* and primary offering, but other headsets
should be implemented — **at least Quest Pro and Quest 3, best effort**. Linux (as a server
platform and ecosystem) is a *desired* goal but **not the focus**: the Linux streaming stack
(Monado/WiVRn/upstream ALVR-Linux) is healthier, and GemLink must not replicate that effort.

## Decision

1. **Device tiers**
   - **Tier 1 — Steam Frame:** primary. Release gates run on Frame scenarios only (ROADMAP).
   - **Tier 2 — Quest Pro, Quest 3:** best-effort parity. Keep codec/foveation/preset paths
     correct for them; light bench scenarios; no release gate depends on them.
   - **Tier 3 — everyone else:** served by the open policy/preset engine; community-contributed
     rows in COMPAT/PRESETS; correctness-by-construction, tested-by-community.
2. **Platform stance**
   - Windows remains the primary server platform (the DDA/IDD source seams are Windows technologies).
   - Linux server: keep upstream's support working (do not break it; accept small fixes) —
     but no Linux-specific roadmap items and no Linux build gates.
   - Linux/Monado client (generic PC or Frame): not planned near-term; the Frame client runs
     on the bundled SteamVR runtime (ADR-0003 §Decision 4, ADR-0009). Revisit post-M3.
3. Capability types stay generic (`client_os`, `decoders`, …) so Tier-2/3 devices are
   first-class citizens of the protocol even while only Frame scenarios gate releases.

## Consequences

- `docs/PRESETS.md` ships with Steam Frame + Quest Pro + Quest 3 rows from day one.
- Bench scenarios: `frame_*` (gating) vs `quest_pro_*` / `quest3_*` (informational tiers).
- Negotiation policy (codec preference AV1 → HEVC10 → HEVC → H264) is device-agnostic;
  device tables only feed *parameters* (resolutions, fps, envelope).
- Revisit trigger: when the Frame path reaches M3, re-evaluate adding a Linux CI tier
  (build-only) so upstream Linux support doesn't rot.
