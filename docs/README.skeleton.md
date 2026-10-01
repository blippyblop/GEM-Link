# <PROJECT NAME>

*A free, open, non-commercial PC-VR streaming stack for Windows — game **and** desktop streaming, virtual displays, an open policy engine — built on [ALVR](https://github.com/alvr-org/ALVR), licensed [MIT](LICENSE) — do what you want.*

No ads. No tiers. No telemetry. Encryption always on.

## Why this fork exists
Upstream ALVR is a SteamVR driver. This project adds what a standalone stack needs — direct desktop capture (DDA), virtual displays (IDD), encrypted transport, hot codec switching, and per-device policy presets — while staying **completely free**. The client is an **OpenXR application on the device's own runtime** (ADR-0009) — we do not build or fork a runtime. See [CHARTER.md](CHARTER.md) and the [ADR log](docs/adr/).

## Status
🚧 Pre-release. See [ROADMAP.md](ROADMAP.md). Compatibility data (bench-generated, honest): [COMPAT.md](COMPAT.md).

## Install
⬜ Releases page → signed installer (server) + store/sideload APK (client). Wired mode supported.

## Bench & development
Every feature is gated by measured scenarios ([docs/BENCH.md](docs/BENCH.md)): frame-delivery tail (missed-deadline %), latency p50/p95, SSIM at equal bitrate. PRs require a scenario. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Provenance & attribution
Built on ALVR (MIT) — see [NOTICE](NOTICE). Per-device presets are interoperability *parameter data* with documented provenance; no proprietary code. This project is not affiliated with alvr-org, Meta, Valve, HTC, or Pico.

## License
MIT (see [LICENSE](LICENSE) and [NOTICE](NOTICE)) — do what you want, attribution included. Our builds are free forever — [funding policy](CHARTER.md#funding-policy).
