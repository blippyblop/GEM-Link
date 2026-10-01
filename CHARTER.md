# CHARTER

## Mission
**GemLink** is a forever-free, open-source, non-commercial PC-VR streaming stack for Windows: **SteamVR is the primary VR path** (ADR-0005, ADR-0009), with desktop streaming, virtual displays, and an open per-device policy engine as the modular second source that works **without SteamVR installed**.

**First-class device: the Steam Frame** — streamed at 90–120 Hz inside a **300 Mbps + gaze-driven foveated-encoding** envelope, tuned for **minimum measured latency**. Gaze-driven foveation is the flagship feature; motion synthesis happens exactly once, at the client. The score is the frame-delivery **tail** — you will not find a target, a gate or a number restated here; they live in [ROADMAP.md](ROADMAP.md) (single source of truth), what we are measured against in [docs/BAR.md](docs/BAR.md), and how we measure in [docs/BENCH.md](docs/BENCH.md).

Built on [ALVR](https://github.com/alvr-org/ALVR) by zarik5 & the alvr-org community; licensed **MIT** — do what you want, attribution included (see NOTICE).

## Values
1. **Free forever** — no ads, no tiers, no paywalls, no telemetry monetization. Donations fund hardware, certificates, and infra only.
2. **Open by construction** — everything (code, presets, benchmarks, compatibility data) is public; decisions are recorded as ADRs.
3. **Measured, not vibes** — every feature lands with a bench scenario (latency, quality, stability gates).
4. **Independence without isolation** — wire-protocol compatibility with upstream is maintained where cheap, so the door back never closes.
5. **User respect** — encryption on by default, no data collection, honest compatibility matrices.

## Non-goals
- Paid features, commercial licensing, marketplace distribution deals.
- Supporting closed platforms that forbid interoperability research.
- Reproducing proprietary products' anti-user choices (obfuscation, lock-in).
- Chasing breadth over depth: other devices ride along at lower priority; the competitive bar and the scope we decline are stated in [docs/BAR.md](docs/BAR.md).
- Being everything: Linux-first users are better served by WiVRn; we are Windows-first, openly.

## Funding policy
Optional donations; published ledger; funds restricted to hardware, signing certificates, and hosting. Maintainers may be reimbursed for documented project expenses. No compensation for code (keeps governance simple and motives clean).

## Trademark
The project name and logo are protected to prevent capture and fake "official" paid builds; the code itself is MIT — do what you want.
