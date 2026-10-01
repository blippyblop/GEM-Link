# x-idd — virtual display for the headset

A Windows **Indirect Display Driver** (UMDF2 + `IddCx`) that creates a virtual
monitor at the headset's resolution. This is the fork's answer to VD's
`MonitorDriver` (`VD_RE/11` item 3: *alvr has no virtual display at all*), and
Phase 2 of the roadmap (`VD_RE/12`: *DDA + IDD virtual displays*).

## Why it exists — it solves four problems at once

| Problem | What x-idd does |
|---|---|
| DDA captures the desktop at 2560x1440; the encoder must be fed VR res | The virtual display **is** the headset resolution — no scaling, no crop |
| DDA refuses with `0x887A0022` when no console session has a live output | A driver-owned output exists independently of the physical desktop |
| alvr has no virtual display (SteamVR game view only) | This is the missing product dimension |
| `DuplicateOutput1` refused (`0x887A0004`, `VD_RE/00`), so 10-bit/HDR capture is impossible | An IDD can advertise advanced colour — the documented "ultimate fix", and the parked 10-bit mystery |

Everything is built by `XIddDriver.vcxproj` with the WDK
(`WindowsUserModeDriver10.0`). Verified building + signing on the box with
WDK 10.0.26100.

## Modes

`Driver.cpp` advertises an **EDID-less** virtual display (no EDID bytes to
craft) with the headset's modes as defaults:

    { 2160, 2160, 90 }, { 2160, 2160, 72 }, { 1920, 1920, 90 },
    { 2560, 1440, 90 }, { 1920, 1080, 60 }

`IDD_SAMPLE_MONITOR_COUNT = 1` — exactly one display, the one the headset
consumes. The OS reports the *intersection* of monitor modes and the target
modes in `MonitorQueryModes`, so both lists must carry the VR modes or nothing
is offered.

## Licensing — read before shipping

`driver/reference/` is **Microsoft's IddSampleDriver** from
`Windows-driver-samples`, which is **MS-PL**, not MIT. The driver in
`driver/` is adapted from it (renamed, monitor count and mode tables changed).
MS-PL permits this **provided the licence is retained**, which is why
`driver/reference/LICENSE-MS-PL` is kept verbatim and the INF carries a
provenance note.

**This makes x-idd a mixed-licence component in an otherwise MIT fork
(ADR-0001).** That is legally fine but is an ADR-level decision; if the fork
must be pure MIT, this driver needs a from-scratch rewrite. Flagged, not
resolved.

## Installing (not yet done)

Requires admin. Two routes:

1. **Test signing** (`bcdedit /set testsigning on`) — **needs a reboot**, which
   would take down the CI runner on this machine.
2. **Self-signed cert in `TrustedPublisher`** — no reboot. Preferred.

`VD_RE/13` warns that IDD/NCM driver installs can bluescreen and that rollback
should be one command. On a daily-driver machine that deserves an explicit
decision rather than a script that runs unattended.
