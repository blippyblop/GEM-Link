# GemLink

*A free, open, non-commercial PC-VR streaming stack, rebuilt ground-up with the **Steam Frame** as its first-class device — game **and** desktop streaming from Windows, virtual displays, gaze-driven foveated encoding, and a single-minded obsession with glass-to-glass latency. Built on [ALVR](https://github.com/alvr-org/ALVR), licensed [MIT](LICENSE) — do what you want, attribution included.*

No ads. No tiers. No telemetry. Encryption always on.

## Target

**Device:** Steam Frame (Qualcomm SM8650 / Adreno 750, 2160×2160 LCD per eye, 72–144 Hz, eye-tracking cameras).
**Links:** Wi-Fi 7 on 6 GHz (the Frame's own SoftAP + dongle, or LAN) · USB-C NCM wired.
**Envelope:** 300 Mbps **with foveated encoding** — gaze-driven foveation is the flagship feature, fixed foveation the fallback.
**North-star metric:** measured glass-to-glass latency. Motion synthesis happens exactly once, at the client — never baked into the PC's encoded frame.

## Why this fork exists

Upstream ALVR is a SteamVR driver maintained at a declining cadence (754→49 commits/yr, 445 commits unshipped past its last release). This project executes the ground-up rebuild upstream cannot absorb: a four-seam architecture (sources → transform → codec plug-ins → transport v2), desktop capture + virtual displays without SteamVR, encrypted frame-agnostic transport, and an open per-device policy engine. See [CHARTER.md](CHARTER.md), [ROADMAP.md](ROADMAP.md) and the [ADR log](docs/adr/).

## Status

🚧 Pre-release, Phase 0 (foundations: protocol crate, capability negotiation, bench). See [ROADMAP.md](ROADMAP.md). Honest, bench-generated compatibility data: [COMPAT.md](COMPAT.md).

## Bench & development

Every feature lands with a measured scenario (`docs/BENCH.md`): latency p50/p95/p99, SSIM at equal bitrate, missed-deadline %. PRs require a scenario. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Provenance & attribution

Built on ALVR (MIT) — see [NOTICE](NOTICE). Per-device presets are interoperability *parameter data* with documented provenance; no proprietary code. Not affiliated with alvr-org, Valve, Meta, HTC, or Pico. "Steam Frame" is a Valve trademark, used to describe compatibility.

## License

MIT (see [LICENSE](LICENSE) and [NOTICE](NOTICE)) — do what you want, attribution included. Our builds are free forever — [funding policy](CHARTER.md#funding-policy).
