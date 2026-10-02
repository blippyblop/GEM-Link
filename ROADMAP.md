# ROADMAP

Status: ✅ done · 🚧 in progress · ⬜ planned · ⛔ blocked (needs hardware) · dates are guidance, **gates are law**.

*This file is the single source of truth for GemLink's goals, its score, and its gates.
Consolidated 2026-10-01 ([ADR-0010](docs/adr/ADR-0010-goals-and-gates-consolidation.md));
other docs link here instead of restating the targets. If a number appears in two
places, this file wins.*

## North star

One device, tuned relentlessly: the **Steam Frame** — 2160×2160/eye LCD (panel
72–144 Hz; the **gating envelope is 90–120 Hz**) — over a **300 Mbps + gaze-driven
foveated encoding** envelope, on Wi-Fi 7 6 GHz (Frame SoftAP + dongle, or LAN) or
USB-C NCM wired, with **minimum measured latency** as the score that outranks all
others.

**Render resolution is the client's call.** The device advertises the size it wants
and the client renders it; GemLink never pins it to a constant. (This is the design,
not a defect — see `VD_RE/20-gemlink-handoff-2026-10-01.md` §6.)

**First shippable target — SteamVR compatibility** ([ADR-0009](docs/adr/ADR-0009-client-runtime-valve-steamvr.md)):
a real SteamVR session on a Windows PC streams end-to-end through GemLink's
`server_openvr` driver, with the device side running as an OpenXR *application* on
Valve's bundled SteamVR runtime. Desktop sources, extra devices and extra transports
are additive and ship after it.

## The score

The score is a **tail, not a mean** — "minimum measured latency". A beautiful average
with 1 % late frames is a broken experience, so frames that blow a deadline outrank
every other number.

- **Mandatory gate — 90 Hz:** **0 %** of frames may miss one 90 Hz frame interval,
  **1000/90 ms = 11.11 ms**.
- **Mandatory gate — never show a bogus frame** ([ADR-0011](docs/adr/ADR-0011-never-show-an-untrusted-frame.md)):
  **0 %** of displayed frames may be frames the client could not reconstruct, at
  **any** source frame rate and **any** induced loss. Holding and reprojecting the
  last good frame is the permitted response; displaying a grey or otherwise
  unfit frame is not. Counted separately from the 90 Hz gate, because a frame can
  be both on time and worthless — which is exactly how this defect hid for a
  session behind a "0 errors" telemetry line.
- **Published target — 120 Hz:** **1000/120 ms = 8.33 ms**; tracked and trended, not
  enforced (`within_optimal_pct`).
- **Secondary:** absolute latency minimized — p50 / p95 / p99 trended downward.
- **Field bar (⛔ hardware-gated):** glass-to-glass **≤ Steam Link / VR Link on
  identical hardware and link**. What we do *not* chase is listed in
  [docs/BAR.md](docs/BAR.md); the measurement method is in [docs/BENCH.md](docs/BENCH.md).

Bench of record: `bench run` → `delivery.missed_mandatory_pct`, `within_optimal_pct`,
`latency.*`. The bench's `latency.*` is *loopback* latency under modelled link
impairment — **not** glass-to-glass. The two are named separately on purpose.

## Definition of done (project-level)

Every item names how it is measured. "Measured" means a `bench` scenario or a named
hardware test — never an opinion. Status is per 2026-10-01.

1. **SteamVR is the primary path; desktop is the modular second source** (ADR-0005,
   ADR-0009).
   *Measured by:* the `frame_*` gating scenarios **and** a hardware session log.
   **Status:** server half ✅ (2026-10-01) · client half ⬜ (M3).
2. **The envelope holds** — 2160²-class presets at 90–120 Hz inside 300 Mbps using
   **gaze-driven foveated encoding** (HEVC/H.264 on the Frame; its kernel decodes
   neither AV1 nor 10-bit today, ADR-0008 — fixed foveation is the fallback).
   *Measured by:* `bench nvenc` bitrate/quality matrix + SSIM at equal bitrate.
   **Status:** 🚧 encode side works; client-side de-foveation not implemented.
3. **The score** (above) — 0 % of frames miss the 90 Hz budget; the 120 Hz target is
   tracked; absolute latency minimized.
   *Measured by:* `bench run` / `bench gate` (`delivery.*`, `latency.*`); field bar per
   `docs/BAR.md`. **Status:** 🚧 loopback green · ⛔ glass-to-glass needs a Frame.
4. **Motion synthesis happens exactly once, at the client** — never baked into the PC's
   encoded stream (the PC-side reprojection wobble anti-pattern, ADR-0003 §3).
   *Measured by:* the design invariant + client-side reprojection tests.
   **Status:** ⬜ (M3).
5. **Hot-switch + encryption** — codec / foveation / bitrate hot-switch mid-session with
   zero dropped frames; **Noise-XX transport default-on end-to-end** — control plane
   *and* media plane (ADR-0006, SECURITY.md).
   *Measured by:* the M4 hot-switch bench; `bench secure` **and** an end-to-end encrypted
   session. **Status:** 🚧 crypto built + gated but **not wired** (M1).
6. **Wired parity** — USB-C NCM reaches the lowest-latency profile and the same presets.
   *Measured by:* the `frame_ncm` scenario + hardware. **Status:** ⬜.

## Milestones

### M0 — Identity & foundations (weeks 1–4) ✅
- [x] Fork at pinned HEAD `9f118394`, tag `base/upstream-9f118394-20260929`
- [x] Kit drop: CHARTER / GOVERNANCE / CONTRIBUTING / SECURITY / NOTICE / COMPAT / ADR-0001..3 / INTEROP / PATCHES
- [x] Identity: GemLink (provisional, ADR-0003); target platform steered (ADR-0003)
- [x] `x-protocol`: versioned protocol + capability negotiation (`decoders`, `foveation_hw`, `link_class`, `client_os`, `max_fps`, display caps)
- [x] Bench walk-skeleton: `x-bench` drives the real control-plane protocol through upstream crates; `metrics.json`; 6 scenarios across 5 impairment profiles (`ncm_wired`, `wifi7_160_clean`, `wifi7_regrace`, `cqm_churn`, `wifi6_lan`)
- [x] CI: Linux tier (build + clippy + fmt + bench gates + secure + gaze) green; Windows GPU tier builds `server_openvr` and runs NVENC 8/10-bit gates
- **Gate (parity release `v0.1.0`):** fork streams ≥ upstream on identical scenarios ✅

### M1 — Latency & trust (weeks 5–12) — **the first shippable target**
- [x] **SteamVR source (`server_openvr`) builds + streams as Source #1** (ADR-0005): real HL2 VR → SteamVR compositor → GemLink driver → NVENC → UDP → real `client_core`, ~70 fps (2026-10-01)
- [x] `x-crypto`: Noise-XX handshake + AEAD transport (ADR-0006), identity fingerprints + pairing pins + tamper tests; `bench secure` gated
- [ ] **Wire the encryption end-to-end** — `SecureControlSocket` into `client_core`/`server_core`, plus media-plane key derivation and per-datagram AEAD. Design trap: the secure control socket holds **one** `NoiseSocket` behind a `Mutex` — it must not go on the video hot path; the media plane needs per-packet nonce/sequence without a shared lock. (Closes DoD #5 / the SECURITY.md promise.)
- [x] Gaze pipeline: OpenXR eye-gaze (Frame) → predicted foveation centers → encoder per-frame centers
- [x] **Gaze tier measured (2026-10-01, `bench gaze` — real driver math, CI-gated):** 90 %-settle **66.7 ms** (2.2× the 30 ms gaze-filter τ), effective sweep-tracking lag **22 ms**, per-sample update cost **~2 µs** (≤100 µs gate)
- [ ] Zero-copy GPU pipeline (DDA texture → encoder, no CPU stage) + per-frame metadata sidecar (timestamps, foveation params, motion vectors)
- [ ] Realtime process priority + high-resolution timer discipline in the capture host (cheap, direct latency)
- [ ] Explain `refresh=72 Hz` — the HMD negotiates 72 against a 90–120 Hz envelope (`steamvr_hmd_init_config`; the harness advertises `[60,72,80,90,120]`)
- **Gate → `v0.2.0` = the first shippable target:** SteamVR path end-to-end (real session → driver → encoder → stream) **and** an end-to-end encrypted session; gaze tracks scripted gaze ≤ 1 frame behind; encode-path drop below M0.
  - *Measured 2026-10-01:* video path ✅ (server half, real game); gaze lag 22 ms ✅; encrypted scenario green ✅ — **wiring still open**.

### M2 — Desktop mode (second source plug-in; weeks 9–20)
- [x] `x-dda`: `DuplicateOutput1` capture built and exercised — `LastPresentTime` gating, immediate release, protected-content refusal
- [x] `x-idd`: indirect display driver built, installed, verified, rolled back; signing path documented
- [ ] `x-dda` HDR formats + suspend/resume; multi-monitor + per-session LUID pinning
- **Gate → `v0.3.0`:** desktop session streamed **without SteamVR installed** (the modular second source; the SteamVR path stays primary)

### M3 — Client & runtime (weeks 15–32) 🚧 **long pole — the device side is at zero**
- [x] **Runtime decided: stay on Valve's SteamVR runtime** (ADR-0009) — the client is an OpenXR application on the bundled runtime; a second runtime is out of scope. No GPL code merge.
- [ ] Frame client binary: `client_core` port to aarch64 Linux — **the Android→Linux port *is* this milestone**: 39 `#[cfg(target_os = "android")]` gates across `client_core` (22), `client_openxr` (11), `system_info` (6). Entry point (`c_api.rs` is JNI-only; `graphics.rs::session_create_info` is `unimplemented!()`), `Platform::SteamFrame`, GLES `SessionCreateInfo` (the vendored `openxr-0.21.1` opengles arm is Android-only), interaction gates
- [ ] **Linux client audio** — `client_core::audio` is `#[cfg(target_os = "android")]` (NDK) and the module only exists on Android. `alvr_audio` already ships a Linux path (`cpal` + `linux.rs::try_load_pipewire()`), so this is plumbing rather than research. Not optional — there is no "audio off" flag.
- [ ] **V4L2 iris decode backend** (`client_core/src/video_decoder` currently has Android/MediaCodec only)
- [ ] Presentation into the Frame's native compositor
- [ ] **Client-side de-foveation + sharpen** — today we spend the foveation bitrate saving and recover nothing; without it the 300 Mbps envelope is not real (DoD #2)
- [ ] Client-side extrapolation (synthesis-once rule), using the Phase-1 sidecar
- **Gate → `v0.4.0`:** GemLink server → Frame client end-to-end on the SteamVR runtime; CTS count published.

### M4 — Transport v2 (parallel to M2/M3)
- [ ] Frame-agnostic typed chunks (video/audio/tracking/events), per-class reliability; **media-plane AEAD** (with M1)
- [ ] Hot codec/config/foveation switch (no teardown); link classification (`wifi7_softap` / `wifi7_lan` / `usb_ncm`)
- [ ] SoftAP-topology aware discovery (server joins the Frame AP or NCM tether); known RF failure modes as bench profiles (reg-race TX cap, CQM churn, GI/LTF pinning)
- **Gate → `v0.5.0`:** mid-session codec switch, zero dropped frames on the impairment profiles

### M5 — Parity pack
- [ ] ViGEm XUSB/DS4 · virtual audio driver + per-process routing · DRM event UX · mic-consent probe
- [ ] `docs/PRESETS.md`: open policy tables (device × tier resolutions, per-codec ladders), provenance-noted; Steam Frame row first
- **Gate → `v0.6.0`:** GemLink feature inventory at "present"

### M6 — Innovation track (continuous)
- [ ] Codec plug-in framework; Vulkan-video study; **hybrid ASTC-fovea + video-periphery** experiment — the 300 Mbps killer feature for UI/text sharpness
- **Gate:** SSIM at equal latency vs HEVC on bench scenarios

## Cadence
`main` stays shippable; a release every 6–8 weeks from M1 (the anti-ALVR lesson). Revisit triggers per ADR-0002.

## Known coverage gaps (fix these, they undermine the gates)
- **Nothing measures trustworthiness of a displayed frame.** The client reported "0 errors, 0 skipped" while 41 % of the frames it displayed were HEVC reconstruction garbage from dropped references (ADR-0011). Until "frames displayed that could not be reconstructed" is counted, the never-show-a-bogus-frame gate has no instrument.
- **CI builds neither `client_core` nor the harnesses** (`x-framesim`, `x-pcsim`) — "main stays shippable" and "measured, not vibes" both have a hole exactly where the newest code lives.
- **No glass-to-glass measurement exists** (no shared clock between PC and client), so DoD #3's field bar is **⛔ blocked on hardware** and every "is it good enough vs X" question is currently unanswerable. See `docs/BENCH.md` §"Not yet measured".
