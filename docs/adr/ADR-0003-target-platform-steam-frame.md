# ADR-0003 — Target platform: Steam Frame; latency & foveation primacy; name GemLink (provisional)

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** solo maintainer (blippyblop)

## Context

The original plan defined a generic Windows→standalone rebuild. The maintainer has now set
primary project goals: the **Steam Frame** as target device, a **300 Mbps + foveated
encoding** envelope, and an **extremely low latency stack from headset to PC** — with
low latency and foveation as the key differentiators. Production streaming stacks already demonstrate which techniques buy minimal latency;
GemLink adopts those conclusions.

Device-level analysis of the Frame (firmware, wireless stack, compositor) is in hand:
SM8650/Adreno 750, 2160² LCD/eye @ 72–144 Hz, **2× eye-tracking cameras**,
Wi-Fi 7 WCN7850 (Frame runs the SoftAP; dongle/PC joins as station), USB-C **NCM
gadget** wired mode, SteamOS-vr aarch64 with bundled SteamVR 2.17.10 (`steamxr_linuxarm64`),
decode via mainline **qcom iris** (V4L2), turnip-only Vulkan, known RF pitfalls
(6 GHz reg-race TX cap, CQM churn, GI/LTF pinning), and a documented PC-streaming
reprojection failure mode (PC-side synthesis bakes stale poses → wobble).

## Decision

1. **Primary device = Steam Frame** (deckard). Other devices remain supported via the
   policy/preset engine but only Frame scenarios gate releases.
2. **Envelope = 300 Mbps with foveated encoding.** Gaze-driven foveation is the
   flagship feature (the hardware has eye trackers; upstream ALVR HEAD already carries
   per-eye runtime centers + OSC gaze — build on it). Fixed foveation is the fallback.
3. **Latency is the primary metric.** Adopt the **synthesis-once rule**: motion
   synthesis/extrapolation happens exactly once, at the client, never in the PC's
   encoded stream (direct lesson from production streaming designs).
4. **Client strategy:** build the Frame client as an OpenXR application against the
   Frame's bundled SteamVR runtime (aarch64); port `client_core` (wgpu→turnip,
   MediaCodec→V4L2/iris decode). A Monado port stays the long-term escape hatch —
   ideas/APIs yes, GPL code never merges (ADR-0001).
5. **Name = GemLink, provisional.** Product identity (docs, README, charter) uses
   GemLink now; mechanical crate-prefix renames are deferred until the name is final
   (amendment to ADR-0002 below). The repo remote stays `GEM-Link`.

### Amendment to ADR-0002 (rename scope)

ADR-0002 said "global rename day one across crates". Amended: **identity-first** —
user-facing identity renames day one; `alvr_*` Cargo package names and the `alvr/`
directory stay until the name is final ("might change later"). Rationale: avoid
double mechanical churn; identity is what etiquette and capture-protection require.
Crate renames remain a one-shot `sed` + `PATCHES.md` entry when executed.

## Consequences

- Phase-0 capability negotiation gains Frame-specific fields: `client_os`
  (`steamos_vr_aarch64`), `decoders` (iris/V4L2: h264/hevc/hevc10/av1/av1_10),
  `foveation_hw` (`eye_gaze`/`fixed`/`none`), `link_class`
  (`wifi7_softap`/`wifi7_lan`/`usb_ncm`).
- Discovery must survive the SoftAP topology inversion (server may join the
  headset's AP) — protocol work item, M0/M4.
- Bench impairment profiles are now modeled on measured Frame RF behavior
  (reg-race, CQM churn, GI/LTF-pinned ceilings) rather than generic wifi6 profiles.
- A known compositor-level risk on Frame (case-2 reprojection) is mitigated by
  decision 3 and monitored with on-device tooling (shim, per-frame counters).
- If the name changes, executing decision 5's deferred rename is the only follow-up.
