# INTEROP — wire compatibility with upstream ALVR

Compatibility with upstream ALVR device↔server interop is **optional and
best-effort** (charter value #4: "the door back never closes"). Anchors we
keep stable so long as it is cheap:

- Discovery: UDP :9943, packet prefix `"ALVR" + 0x00 x 12`, 8-byte protocol
  ID derived from semver, 32-byte hostname field (see wiki/How-ALVR-works.md).
- Control socket: TCP; stream socket: UDP/TCP with MTU shards.
- Protocol versioning: semver-compatible client↔streamer ⇒ matching ID.

If/when transport v2 lands, it negotiates downgrade to the
legacy wire format where feasible; where not feasible, `COMPAT.md` gains a
"legacy mode" row per device.
