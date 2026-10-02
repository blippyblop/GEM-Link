# INTEROP — historical (upstream wire compatibility is no longer a goal)

> **Status: historical as of 2026-10-02.** This file described a stance — keep the
> wire compatible with upstream ALVR "where cheap", so the door back never closes —
> that was superseded when upstream was declared abandoned
> ([ADR-0012](docs/adr/ADR-0012-upstream-alvr-is-abandoned.md)). GemLink speaks
> GemLink's protocol. The content below is kept because it is an accurate
> description of the framing the inherited crates still emit today, and because
> anyone porting an ALVR build across will want to know what changed and when.

## What the inherited transport actually does

- Discovery: UDP :9943, packet prefix `"ALVR" + 0x00 x 12`, 8-byte protocol ID
  derived from semver, 32-byte hostname field.
- Control socket: TCP; stream socket: UDP/TCP with MTU shards.
- Protocol versioning: semver-compatible client↔streamer ⇒ matching ID.

## What this means now

- **No downgrade obligation.** Transport v2 does not need to negotiate a fallback
  to the legacy wire format. The "legacy mode row per device" that used to be
  promised here is not owed.
- **Compatibility is a debugging convenience, not a contract.** Being able to
  point an ALVR client at a GemLink server (or the reverse) remains occasionally
  useful for isolating which side of a fault you are looking at. It is not a
  feature anyone is entitled to, and it will break as the transport is replaced.
- **`COMPAT.md` is about devices, not protocols.** Its rows describe what a
  headset can do with GemLink; nothing in it depends on the framing above.
