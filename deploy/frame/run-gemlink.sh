#!/bin/sh
# GemLink client launcher on the Steam Frame.
#
# Mirrors Valve's run_vrlink.sh shape: cd to the install dir, default the
# environment the runtime needs, exec the client so signals land on it directly.
cd "$(dirname "$0")" || exit 1

# The OpenXR loader is not on the default search path (upstream's client only
# found it because a Java host set it — our replacement is this variable).
: "${ALVR_OPENXR_LOADER:=/opt/steamvr/bin/linuxarm64/libopenxr_loader.so}"
export ALVR_OPENXR_LOADER

# The bundled SteamVR runtime manifest — this is the source of OpenXR (ADR-0009).
: "${XR_RUNTIME_JSON:=/opt/steamvr/steamxr_linuxarm64.json}"
export XR_RUNTIME_JSON

# Valve's own launcher sets this for the Turnip driver.
: "${TU_DEBUG:=gmem}"
export TU_DEBUG

# Smoke ladder + bring-up stages are info-level; without this they are silent.
: "${RUST_LOG:=info}"
export RUST_LOG

# Exit codes from main.rs: 2 = no display connection (environment wrong),
# everything else is the runtime's or a panic. The launcher surfaces them as-is
# so a systemd unit or the UI can tell why we stopped.
exec ./alvr_client_openxr
