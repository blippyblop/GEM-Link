# ROADMAP

Status: ✅ done · 🚧 in progress · ⬜ planned · dates are guidance, **gates are law**.

## North star

One device, tuned relentlessly: the **Steam Frame** —
2160×2160/eye, 90–120 Hz, over a **300 Mbps + foveated encoding** envelope,
on Wi-Fi 7 6 GHz (Frame SoftAP + dongle, or LAN) or USB-C NCM wired,
with **minimum measured glass-to-glass latency** as the score that outranks all others.

## Definition of done (project-level)

1. A Steam Frame streams PC VR **and** the Windows desktop from a PC with **SteamVR not installed**.
2. 2160²-class presets at 90–120 Hz hold inside the 300 Mbps envelope using **gaze-driven foveated encoding** (AV1/HEVC10), fixed foveation as fallback.
3. Bench-measured glass-to-glass ≤ Steam Link/VR Link on identical hardware and link; no wobble — motion synthesis happens **once, client-side** (never baked into encoded PC frames).
4. Codec / foveation / bitrate hot-switch mid-session, zero dropped frames; Noise-encrypted transport default-on.
5. Wired USB-C NCM reaches the same presets as the lowest-latency profile.

## Milestones

### M0 — Identity & foundations (weeks 1–4)
- [x] Fork at pinned HEAD `9f118394`, tag `base/upstream-9f118394-20260929`
- [x] Kit drop: CHARTER / GOVERNANCE / CONTRIBUTING / SECURITY / NOTICE / COMPAT / ADR-0001..3 / INTEROP / PATCHES
- [x] Identity: GemLink (provisional, ADR-0003); target platform steered (ADR-0003)
- [ ] `x-protocol` crate: versioned protocol + capability negotiation (`decoders`, `foveation_hw`, `link_class`, `client_os`, `max_fps`, display caps)
- [ ] CI: Linux tier (build + clippy + bench-loopback) green; Windows GPU tier stub
- [x] Bench walk-skeleton: `x-bench` speaks the real control-plane protocol by driving upstream crates; `metrics.json`; 6 scenarios across 5 impairment profiles (`ncm_wired`, `wifi7_160_clean`, `wifi7_regrace`, `cqm_churn`, `wifi6_lan`)
- **Gate (parity release `v0.1.0`):** fork streams ≥ upstream on identical scenarios

### M1 — Latency & trust (weeks 5–12)
- [ ] Zero-copy GPU pipeline (DDA texture → encoder, no CPU stage) + per-frame metadata sidecar (timestamps, foveation params, motion vectors)
- [ ] Noise-XX pairing + AEAD transport, default ON (no plaintext mode)
- [ ] Realtime priority + high-resolution timer discipline in the capture host
- [ ] Gaze pipeline: OpenXR eye-gaze (Frame) → predicted foveation centers → encoder per-frame centers
- **Gate:** measured encode-path drop vs M0; encrypted bench scenario green; foveation tracks scripted gaze ≤ 1 frame behind → `v0.2.0`

### M2 — Desktop mode (weeks 9–20)
- [ ] `x-dda`: DuplicateOutput1 HDR formats, LastPresentTime gating, suspend/resume, protected-content stop, cursor path
- [ ] `x-idd`: indirect display driver, AddIfNecessary(count), portrait; signing path documented
- [ ] Multi-monitor + per-session LUID pinning
- **Gate:** desktop session with SteamVR uninstalled, streamed to Frame → `v0.3.0`

### M3 — Client & runtime (weeks 15–32) 🚧 long pole
- [ ] Frame client: OpenXR app on the Frame's bundled SteamVR runtime (aarch64); `client_core` port (wgpu/turnip, V4L2-iris decode path)
- [ ] ADR: Monado-adopt vs stay-on-Valve-runtime (OpenXR-CTS counter in CI either way); no GPL code merge
- [ ] Client-side extrapolation (synthesis-once rule): depth/motion-vector assisted, using Phase-1 sidecar
- **Gate:** GemLink server → Frame client end-to-end with SteamVR absent on PC; CTS count published → `v0.4.0`

### M4 — Transport v2 (parallel to M2/M3)
- [ ] Frame-agnostic typed chunks (video/audio/tracking/events), per-class reliability
- [ ] Hot codec/config/foveation switch (no teardown); link classification (`wifi7_softap` / `wifi7_lan` / `usb_ncm`)
- [ ] SoftAP-topology aware discovery (server joins Frame AP or NCM tether); known RF failure modes encoded as bench profiles (reg-race TX cap, CQM churn, GI/LTF-pinned ceilings)
- **Gate:** mid-session codec switch, zero dropped frames on impairment profiles → `v0.5.0`

### M5 — Parity pack
- [ ] ViGEm XUSB/DS4 · virtual audio driver + per-process routing · DRM event UX · mic-consent probe
- [ ] `PRESETS.md`: open policy tables (device × tier resolutions, per-codec ladders) — provenance-noted; Steam Frame row first
- **Gate:** GemLink feature inventory at "present" → `v0.6.0`

### M6 — Innovation track (continuous)
- [ ] Codec plug-in framework; Vulkan-video study; **hybrid ASTC-fovea + video-periphery** experiment — the 300 Mbps killer feature for UI/text sharpness
- **Gate:** SSIM @ equal latency vs HEVC on bench scenarios

## Cadence
`main` stays shippable; a release every 6–8 weeks from M1 (the anti-ALVR lesson). Revisit triggers per ADR-0002.
