# ADR-0009 — Client runtime: stay on Valve's SteamVR runtime; SteamVR compatibility is the first shippable target

Status: Accepted (2026-10-01)
Deciders: maintainer

## Context

ADR-0005 §3 shelved "own OpenXR runtime" ambitions, but it did so on the PC
*source* side, and ROADMAP M3 still carried an open ADR — "Monado-adopt vs
stay-on-Valve-runtime". Leaving the client-side runtime question undecided is
not neutral: it invites effort on a second runtime that the product will never
ship, which is development load spent for zero product value.

The device ships a complete, maintained VR runtime (aarch64) whose OpenXR
runtime is part of it, and PC streaming is a first-class mode of that runtime.
Adopting a second runtime means owning conformance, a compositor, reprojection,
and device drivers — all of which already exist and are maintained by someone
else.

## Decision

1. **The client targets Valve's SteamVR runtime on the device.** It is an
   OpenXR *application* on the bundled runtime; it does not replace it, wrap it,
   or sit beside it. The runtime question in ROADMAP M3 is closed:
   **stay on Valve's runtime**.
2. **SteamVR compatibility is the first shippable target.** Shippable means: a
   real SteamVR session on a Windows PC streams end-to-end through GemLink's
   SteamVR driver (Source #1, ADR-0005), with the device side running on the
   bundled runtime. Everything else — desktop capture sources, other devices,
   other transports — is additive and ships afterwards.
3. **Adopting a second runtime (e.g. Monado) is out of scope**, recorded as
   revisit-only-if-blocked at the same bar as ADR-0005 §3. No GPL code merge
   (charter rule): ideas and APIs only, never source.
4. **Development and emulation target the Valve runtime.** A harness that stands
   up an OpenXR runtime must stand up *that* runtime — or drive the same
   protocol path — otherwise it measures compatibility with something the
   product will never meet. A harness that deliberately omits a runtime is fine
   for the transport/protocol plane and must be labelled as such rather than
   quietly treated as client coverage.

## Consequences

- Client scope is an OpenXR app + codec/decode + the streaming pipeline.
  Compositor, reprojection, and device drivers are the runtime's job.
- The device runtime is not a build dependency and never enters CI.
- Emulation effort is redirected to **SteamVR compatibility on the PC side** —
  the real driver loaded by a real SteamVR session, real encoder seam, real
  foveation ABI — which is testable with no device present, plus a *bounded*
  probe of the device runtime under emulation.
- The protocol-level harnesses (which have no runtime by design) keep their
  value for transport, negotiation, crypto and timing, and stay in the loop.
- Revisit trigger: the runtime concretely blocks a measured goal (latency,
  codec, foveation, or a required extension). Re-open with evidence, not
  preference.
