# COMPAT — Device compatibility matrix

Generated from bench + field reports. ✅ verified · 🟨 works w/ caveats · 🚧 partial · ⬜ untested · ❌ known-broken.

Targets and gates are defined once in [ROADMAP.md](ROADMAP.md); how the numbers are produced is in [docs/BENCH.md](docs/BENCH.md). An untested cell is `⬜`, not a claim.

| Device | Client build | Res ladder | Refresh | Codecs | Foveation | Tracking | Notes |
|---|---|---|---|---|---|---|---|
| Meta Quest 3 | ⬜ | ✅ presets | 90/120 | AV1/HEVC10 | ✅ | ⬜ |  |
| Meta Quest 2 | ⬜ | ✅ presets | 90/120 | HEVC10 | ✅ | ⬜ |  |
| Quest Pro | ⬜ | ✅ presets | 90 | HEVC10 | ✅ eye | ⬜ | eye-tracked foveation |
| Pico 4 / Ultra | ⬜ | ✅ presets | 72/90 | HEVC10/AV1(U) | ✅ | ⬜ |  |
| Vive Focus 3 / XR Elite | ⬜ | ✅ presets | 90 | HEVC10 | ✅ | ⬜ | upstream notes "laggy" |
| Steam Frame | ⬜ | ✅ presets | 90–120 | HEVC/H264 | 🚧 eye | ⬜ | **first-class device**; no AV1/10-bit (ADR-0008); client unbuilt (M3) |
| Galaxy XR | ⬜ | ✅ presets | ⬜ | ⬜ | ⬜ | ⬜ |  |
| Desktop (virtual display) | ⬜ | M2 gate | any | all | n/a | n/a |  |

## GPU matrix (server)
| GPU | Encode paths | Notes |
|---|---|---|
| NVIDIA (Turing+) | NVENC AV1/HEVC/H264 | P4 default tuning |
| AMD RDNA | AMF / Vulkan-video | VAAPI code is Linux-only upstream — do not ship |
| Intel | QSV | verify AV1 (Arc) |
| Software | x264/SW AV1 | CI tier only |

*Resolutions and bitrate ladders: see `PRESETS.md` (per-device × quality-tier tables; provenance-noted parameter data).*
