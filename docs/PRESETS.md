# PRESETS — open per-device policy tables

**Provenance:** these tables are interoperability *parameter data* — device capability and quality-tier starting points.
No proprietary source code is used or included. Values are starting points:
the bench and field reports (`COMPAT.md`) update them over time.

**Envelope:** GemLink's gating scenarios run the Steam Frame at **300 Mbps with
gaze-driven foveated encoding** (ADR-0003). Ceiling rows below are client decode
ceilings, not our default. Targets and gates are in [../ROADMAP.md](../ROADMAP.md).

## Device table

| Device | Tier | Panel | Refresh (Hz) | Codec preference | Foveation | Ceiling bitrate (default / max) | GemLink scenario default |
|---|---|---|---|---|---|---|---|
| **Steam Frame** | 1 | 2160² LCD/eye | panel 72–144 · envelope **90–120** | HEVC → H264 (AV1/10-bit: kernel-limited nice-to-have, ADR-0008) | **EyeGaze** (2× IR cams) | 200 / 500 (H264+HP) | 300 Mbps foveated, HEVC/H264, 90–120 Hz (`frame_wifi7_160`, `frame_ncm`) |
| **Quest Pro** | 2 | ~1832×1920/eye | 72 / 90 | HEVC10 → H264 | EyeGaze | 200 / 200 | 200 Mbps, HEVC10, 90 Hz (`quest_pro_wifi6`) |
| **Quest 3** | 2 | 2064×2208/eye | 72 / 80 / 90 / 120 | AV1 → HEVC10 → H264 | Fixed | 200 / 200 (H264+HP 500–600) | 200 Mbps, AV1, 90 Hz (`quest3_wifi6`) |

## Resolution ladders (per eye, quality tiers)

- **Steam Frame** (square group: XR Elite / Pico 4 / Pico 4 Ultra / Steam Frame):
  1344² · 1728² · 2112² · 2688² · 2880² · 3264² · 3840² · 4416² · 5184²
  ("Comfort" 1728² · "Quality" 2688² · "Max" 4416²)
  - The panel is **native 2160²/eye**, which is not a rung above — the ladder is a
    VD-derived *starting set*. The device advertises the render size it wants and the
    **client chooses** (never pinned to a constant; see ROADMAP north star).
- **Quest Pro / Quest 3** (current-gen group: Quest 2/3/3S/Pro, Focus 3, Neo 3, Galaxy XR):
  1344×1440 · 1728×1824 · 2112×2304 · 2496×2688 · 2688×2880 · 3072×3264 · 3648×4032 · 4224×4608 · 4992×5376
  (Quest Pro "Quality" 2496×2688 · Quest 3 "Quality" 2688×2880)

## Notes & caveats

- Bitrate caps are **client** decode ceilings; the 300 Mbps GemLink
  envelope sits inside the Frame's 500 Mbps H264+ ceiling and above the 200 default.
- 10-bit AV1 decode on the Frame's qcom iris decoder is **unverified** — protocol
  samples deliberately do not advertise `Av110Bit` for the Frame yet — pending
  device capability validation. Capability negotiation handles it automatically once verified.
- Foveation fallback ladder: EyeGaze → head-gaze → Fixed → Disabled. Fixed-foveation
  geometry (center size/shift/edge ratio) reuses upstream ALVR's parameters.
- Wired USB-C NCM is the lowest-latency profile on every device that supports it
  (Frame ships the NCM gadget); scenario `*_ncm` asserts the latency win.
