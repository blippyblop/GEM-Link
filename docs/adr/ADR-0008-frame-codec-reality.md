# ADR-0008 — Steam Frame decode reality: no AV1, no 10-bit on the current kernel

Status: Accepted (2026-09-30)
Deciders: maintainer (device finding)

## Context

GemLink's codec preference policy assumed AV1-first for the Steam Frame. The
Frame's current kernel does not expose AV1 or 10-bit decode (iris V4L2 path).
These become **nice-to-haves** gated on future firmware/kernel updates, not
requirements.

## Decision

1. Frame capability samples advertise **HEVC 8-bit and H.264 only** (HEVC
   8-bit pending on-device verification; H.264 is the guaranteed floor).
2. Codec preference policy is unchanged globally — AV1/10-bit remain first
   choices for devices that decode them (Quest 3 etc.). Per-device capability
   tables carry the truth.
3. **Silver lining**: H.264 has the lowest decode latency of the supported
   set, aligned with the latency-first charter. The 300 Mbps envelope holds.
4. x-nvenc keeps Main10/AV1 encode support — the PC side is unaffected, and
   future firmware may unlock the device side.

## Consequences

- PRESETS.md Frame row updated (HEVC → H264; AV1/10-bit marked nice-to-have).
- ROADMAP definition-of-done wording updated to HEVC/H.264 for the Frame.
- Revisit trigger: Frame firmware/kernel update adding AV1 or 10-bit decode —
  flip the capability sample back and regenerate goldens.
