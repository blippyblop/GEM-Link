# BAR — what GemLink is measured against

*The competitive bar has to be written down or it drifts. This file says who we are
compared to, how the comparison is made, and what we deliberately decline to chase.
The score itself is in [../ROADMAP.md](../ROADMAP.md#the-score); the method is in
[BENCH.md](BENCH.md).*

## The bar

There are two products we are measured against, and they are not the same bar:

- **Valve's own PC-side Frame streaming driver** (`vrlink` / `SVLHMDDriverD3D11` /
  `SVLServer`) — the **shipping default** for this device. This is the bar that matters
  most, because it is what a Frame owner already has. It is installed on the test box
  and speaks the same SteamVR sourcing path we do (ADR-0005).
- **Virtual Desktop** — the third-party benchmark users actually compare against.
  **It is a single-developer product** (Guy Godin / Virtual Desktop, Inc.), closed
  source, ~$20. Stated plainly because it changes how the bar reads: the breadth we are
  measured against was built by **one person**, so there is no headcount excuse
  available to us — and, equally, it proves the bar is reachable by an individual.

**Success is:** GemLink is **at least as good as these on the Steam Frame's core
streaming experience** — the same frames, at the same or better quality, with a
**lower tail latency**, inside the same 300 Mbps envelope, with encryption on. If it
is not, the project has failed (this was stated explicitly and is the project's
working standard, not an aspiration).

## How the comparison is made

Identical hardware and link, our own score, no cherry-picking:

- **Metric:** the tail — 99.99 % of frames inside the frame budget, then absolute
  latency p50/p95/p99 (ROADMAP "The score"). Never the mean.
- **Quality:** SSIM at equal bitrate, and the bitrate needed to reach equal SSIM.
- **Envelope:** 300 Mbps **with foveated encoding** for the Frame; without foveation
  a quality dip is expected and is not a loss.
- **Method:** glass-to-glass on a shared clock (or a light-to-photon rig) — **not yet
  built**; see [BENCH.md](BENCH.md#planned-not-yet-built). Until it exists the
  comparison is a **⛔ hardware-gated** claim, not a measured one, and is labelled that
  way everywhere.

## What we do not chase

Declined **on the record**, so scope is a decision rather than a drift. These are not
"never"; they are "not part of the bar we are scored on":

- **Breadth over depth.** VD's per-device × per-GPU-tier policy matrices (~17 headsets
  × 9 GPU tiers), multi-platform clients, and store/entitlement plumbing. We tune
  **one device** hard and keep the policy engine open; other devices ride along at
  lower priority (ADR-0004).
- **An updater and distribution machinery** as a parity goal.
- **Reproducing proprietary anti-user choices** (obfuscation, lock-in) — CHARTER
  non-goals.
- **Being everything to everyone.** Linux-first users are better served by WiVRn; we
  are Windows-first, openly (CHARTER).

## Where the current stack stands vs VD

Tracked in `VD_RE/11-vd-features-missing-from-alvr.md` (the gap inventory) and
`VD_RE/00-HANDOFF.md` (the status review). Headline, 2026-10-01: we are **at parity on
the VR capture → encode → transport → tracking path**; we are **off on the device side
(unbuilt), frame extrapolation, encrypted transport (built, unwired), client-side
de-foveation, latency hygiene, USB-as-NIC, policy data, and shipping**. The risky third
is done; the rest is a known list, not a research programme.
