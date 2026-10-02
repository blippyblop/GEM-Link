# PATCHES — provenance record for shared crates

One line per deviation to the inherited ALVR crates (`sockets`, `packets`,
`session`, `graphics`, `server_core`, `client_*`, `dashboard`, `common`, `events`,
`filesystem`, `server_io`, `system_info`, `adb`, `audio`, `launcher`,
`vulkan_layer`, `vrcompositor_wrapper`, `xtask`).

**This is no longer a rebase budget.** Upstream is abandoned
([ADR-0012](docs/adr/ADR-0012-upstream-alvr-is-abandoned.md)): there is nothing to
rebase onto and no structural symmetry left to protect, so the file's job changed
from "keep this small" to **"be able to tell our code from inherited code when
debugging"**. Entries are recorded as they are found, not rationed. New work still
belongs in an `x-*` crate, for the ordinary reason that a standalone crate is
easier to reason about — not to protect a merge that is not coming.

| # | Crate | Deviation | Reason | ADR/issue |
|---|-------|-----------|--------|-----------|
| 1 | `session` | `ConnectionSettings::wlan_optimizer: bool` (default `true`) + help text | The user-facing switch for `x-link`'s WLAN optimizer. The mechanism is a new `x-*` crate; only the setting has to live in the schema so the dashboard can toggle it. | `ROADMAP.md` M4 · `VD_RE/24-vd-link-qos.md` |
| 2 | `server_core` | `connection.rs::connection_pipeline` starts an `x_link::WlanSession` guard for the life of the session, gated on `connection.wlan_optimizer` | The session lifecycle is upstream's, so the "while a session runs" hook has to sit in it. The guard is RAII: dropped — and the adapter restored — on every return path, including a failed handshake. | as above |
| 3 | `session` | `ConnectionSettings::host_scheduling: bool` (default `true`) + help text | The switch for `x-link::sched` (priority class, EcoQoS opt-out, timer resolution). | as above |
| 4 | `session` | `connection.dscp` default changed from unset to `ExpeditedForwarding` | Media is the latency-critical flow; marking it is free and the default was for nobody to mark anything. | `ROADMAP.md` M4 |
| 5 | `session` | `DropProbability::{Medium, High}` changed from `0x10`/`0x11` to `0b10`/`0b11` | Hex literals where binary ones were meant: `Medium` was sixteen, so it landed inside the assured-forwarding class field. Serialised by variant name, so stored settings are unaffected. | `alvr_sockets` tests |
| 6 | `sockets` | `set_dscp` split into a testable `dscp_to_tos`, with the drop precedence given its `<< 1` | The AF branch was arithmetically wrong for every combination: AF11 was 9 rather than 10. It was inline, so untested, so wrong. | as above |
| 7 | `server_core` | `connection.rs::connection_pipeline` resolves the link once (`x_link::resolve`), logs it, and derives the QoS posture from it; starts an `x_link::HostScheduler` guard | The resolution has to happen where the peer address is known, which is the session lifecycle. Both guards are RAII and are dropped on every return path. | `ROADMAP.md` M4 · `VD_RE/24-vd-link-qos.md` §9 |
| 8 | `sockets` | `secure_control_socket.rs` tests: dropped two `map_err(\|e\| e)` | Pre-existing clippy `map_identity` failures that made `--all-targets` unusable for the crate. | — |

## Known-unlisted deviations (audit needed)

This table claimed "none yet" until 2026-10-02. That was wrong, and a record that
under-reports is worse than no record, so they are noted here rather than silently
omitted. Both predate the rows above:

- **`packets`, `server_core`, `client_core`, `x-pcsim`, `x-framesim`** — the monotonic
  `frame_index` carried from `VideoPacketHeader` through `send_video_nal` and checked for
  gaps on the client. That is [ADR-0011](docs/adr/ADR-0011-never-show-an-untrusted-frame.md)'s
  required signal, so it is *expected* to deviate; it is listed because "expected" is not
  "free at rebase time".
- **`session`, `server_core`, `server_openvr`, `common`** — `ControllersEmulationMode::SteamFrame`,
  the `FRAME` interaction profile and its button vocabulary, the Steam Frame driver props,
  and the restart-hash discriminator. The first-class device is the reason the fork exists
  ([ADR-0003](docs/adr/ADR-0003-target-platform-steam-frame.md)).

Neither has been re-verified against upstream since it landed. With upstream
abandoned ([ADR-0012](docs/adr/ADR-0012-upstream-alvr-is-abandoned.md)) there is no
longer any value in *re-verifying* them — the value is in them being listed, so that
when something in `packets`/`server_core`/`session` misbehaves we know whether we are
reading inherited code or ours. The audit TODO is therefore about **completeness**,
not about reducing surface.

| 9 | `server_core` | `send_video_nal` replaces the `STREAM_CORRUPTED` static and the `avoid_video_glitching` bypass with `x_transport::SendGate`; the mirror and rolling-file recording stay inside the gate | ADR-0011's send half. The old code discarded a frame and kept transmitting the P-frames that followed, which cannot be reconstructed. | [ADR-0011](docs/adr/ADR-0011-never-show-an-untrusted-frame.md) |
| 10 | `client_core` | the video receive loop replaces `stream_corrupted` + its own gap arithmetic with `x_transport::TrustGate`; the gap now arms the gate, an untrusted frame is never submitted, and a keyframe is requested once per recovery instead of per frame | The display half. The gate was armed on datagram loss only, which a server-side discard does not produce — so 41 % of displayed frames could be undecodable while every counter read zero. | as above |
| 11 | `session` | `connection.avoid_video_glitching` default `false` → `true`, and its help text rewritten | It no longer gates the invariant (it cannot), so the old text and default were actively misleading. It now controls whether the client spends a keyframe request to shorten a hold. | as above |
