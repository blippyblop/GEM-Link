# ADR-0005 — SteamVR is the primary source; the source seam stays modular

Status: Accepted (2026-09-30)
Deciders: maintainer

## Context

The roadmap angled toward desktop-first streaming with an eventual own runtime.
The maintainer has now set product direction: **streaming VR games through
SteamVR is the primary use case.** SteamVR exists and is excellent — rebuilding
it is explicitly out of scope. Other sources must remain modular additions,
never compromises to the primary path.

## Decision

1. **The SteamVR driver architecture (upstream `server_openvr`) is Source #1
   and the primary product path.** ADR-0002 §4 ("do not build server_openvr")
   is amended: it builds, ships, and is maintained as GemLink's main PC-side
   source. Patch discipline (PATCHES.md) keeps rebases cheap.
2. **All roadmap work stays behind the source seam.** Desktop capture (DDA) and
   virtual displays (IDD) are *additional* sources feeding the same
   transform → codec → transport pipeline. They are additive, never a
   replacement for the SteamVR path.
3. **"Own OpenXR runtime" ambitions are shelved.** Revisit only if SteamVR
   concretely blocks a measured goal.
4. **The encoder seam serves all sources identically.** SteamVR textures go
   through the same zero-copy registration path (x-nvenc) and the same
   delivery gates as desktop frames.

## Consequences

- `server_openvr` enters the Windows build matrix (C++ toolchain + SteamVR
  SDK; the reference box has VS BuildTools).
- Desktop work (x-dda/x-idd) continues as the modular second source; its gates
  remain, but product emphasis sits behind the SteamVR path.
- Latency/quality gates are source-agnostic: the SteamVR path is held to the
  same two-tier delivery standards as everything else.
