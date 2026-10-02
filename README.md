# GemLink

**IMPORTANT** : This project is going to get parked and development is going to slow massively because of this: FRAME STREAM (also styled as Frame Stream), developed by @ji10me (ジトメ / JTOME on VRChat).

Basically someone else already made what I was trying to put together; and chances are they actually have a device already. I'll use this project as a dump project for tokens since it'd be cool to get an open source project for this; also if FRAME STREAM is closed source this project will keep moving but priority will be low as functionality is the primary focus. 

*A free, open, non-commercial PC-VR streaming stack, rebuilt ground-up with the **Steam Frame** as its first-class device — game **and** desktop streaming from Windows, virtual displays, gaze-driven foveated encoding, and a single-minded obsession with the latency **tail**. Built on [ALVR](https://github.com/alvr-org/ALVR), licensed [MIT](LICENSE) — do what you want, attribution included.*

No ads. No tiers. No telemetry. Encryption always on.

## Target

**Device:** Steam Frame (Qualcomm SM8650 / Adreno 750, 2160×2160 LCD per eye, panel 72–144 Hz; the gating envelope is 90–120 Hz; eye-tracking cameras). Render resolution is the **client's** call.
**Links:** Wi-Fi 7 on 6 GHz (the Frame's own SoftAP + dongle, or LAN) · USB-C NCM wired.
**Envelope:** 300 Mbps **with gaze-driven foveated encoding** — the flagship feature; fixed foveation the fallback. Decode is **HEVC/H.264** (the Frame kernel decodes neither AV1 nor 10-bit today).
**North-star metric:** the **tail**, not the mean — **0 % of frames over the 90 Hz budget (1000/90 ms)**, the 120 Hz budget (1000/120 ms) as the published target, absolute latency minimized after that. Motion synthesis happens exactly once, at the client — never baked into the PC's encoded frame.

Single source of truth for targets and gates: [ROADMAP.md](ROADMAP.md). What we are measured against: [docs/BAR.md](docs/BAR.md). How we measure: [docs/BENCH.md](docs/BENCH.md).

## Why this fork exists

Upstream ALVR is a SteamVR driver maintained at a declining cadence (754→49 commits/yr, 445 commits unshipped past its last release). This project executes the ground-up rebuild upstream cannot absorb: a four-seam architecture (sources → transform → codec plug-ins → transport v2), desktop capture + virtual displays without SteamVR, encrypted frame-agnostic transport, and an open per-device policy engine. See [CHARTER.md](CHARTER.md), [ROADMAP.md](ROADMAP.md) and the [ADR log](docs/adr/).

## Status

🚧 Pre-release. The **server half of the first shippable target works** — a real SteamVR game streams end-to-end through GemLink's `server_openvr` driver (2026-10-01); the Steam Frame client is the long pole and is next. See [ROADMAP.md](ROADMAP.md) and [docs/BAR.md](docs/BAR.md). Honest, bench-generated compatibility data: [COMPAT.md](COMPAT.md).

## Bench & development

Every feature lands with a measured scenario ([docs/BENCH.md](docs/BENCH.md)): frame-delivery tail (missed-deadline %), latency p50/p95/p99, SSIM at equal bitrate. PRs require a scenario. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Provenance & attribution

Built on ALVR (MIT) — see [NOTICE](NOTICE). Per-device presets are interoperability *parameter data* with documented provenance; no proprietary code. Not affiliated with alvr-org, Valve, Meta, HTC, or Pico. "Steam Frame" is a Valve trademark, used to describe compatibility.

## License

MIT (see [LICENSE](LICENSE) and [NOTICE](NOTICE)) — do what you want, attribution included. Our builds are free forever — [funding policy](CHARTER.md#funding-policy).
