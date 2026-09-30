# ADR-0006 — Encryption always-on; insecure-debug transport is compile-time only

Status: Accepted (2026-09-30)
Deciders: maintainer

## Context

The charter and SECURITY.md promise encryption always on, no plaintext mode.
The maintainer also has a legitimate engineering need: capturing the wire
protocol in the clear while debugging it. A **runtime** toggle would satisfy
the second and endanger the first — toggles leak into shipped defaults.

## Decision

1. **Noise-XX (25519 / ChaChaPoly / SHA256) is the transport security
   layer**, implemented in `x-crypto` on top of the `snow` framework.
   Identities are static 25519 keypairs pinned at pairing time (fingerprint =
   SHA-256 key prefix, compared by humans on both screens); the handshake
   verifies the received peer key against the pin, making man-in-the-middle
   substitution fail closed.
2. **A plaintext transport exists only under the cargo feature
   `insecure-debug-transport` (default OFF).** It is a compile-time toggle:
   release builds contain no plaintext path to accidentally enable, and the
   bench can assert the posture (`encryption_always_on()`).
3. **Negotiation**: capabilities carry `insecure_debug`; the session plan
   selects `InsecureDebugOnly` only when BOTH sides advertise it, else
   `NoiseXx`. Release binaries hard-code `insecure_debug = false`.
4. **Upstream wire compatibility note**: upstream ALVR's media path is
   unencrypted, so encrypted GemLink-to-GemLink sessions are not
   upstream-interoperable; the interop posture in INTEROP.md is best-effort
   and stays secondary to the security posture. (Upstream protocol
   compatibility at the session layer remains where cheap.)

## Consequences

- Debugging the wire requires rebuilding with `--features insecure-debug-transport`
  — an explicit, deliberate act.
- CI keeps a test asserting the default posture.
- Integration into the control/media planes lands next (M1); the fake headset
  runs the same handshake so the encrypted bench scenario is protocol-faithful.
