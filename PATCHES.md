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
