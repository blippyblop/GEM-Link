# Deploying the GemLink client to a Steam Frame (aarch64 Linux)

The device is an aarch64 Arch-based SteamOS variant with sshd in-image. Install
over SSH; the app appears in the headset's own launcher.

## What ships

| file | goes to | what |
|---|---|---|
| `alvr_client_openxr` | `/opt/gemlink/` | the client (stripped release build) |
| `run-gemlink.sh` | `/opt/gemlink/` | launcher: loader path, runtime manifest, `RUST_LOG=info` |
| `gemlink.desktop` | `/usr/share/applications/` | the entry in the headset's app grid |

## Steps (from the workstation)

```sh
# 1. Build (in the container, at the commit both sides share):
#    cargo build --release --target aarch64-unknown-linux-gnu -p alvr_client_openxr
#    zig objcopy --strip-all target/.../release/alvr_client_openxr deploy/frame/alvr_client_openxr

# 2. Copy to the device
scp -r deploy/frame steamos@DEVICE_IP:/tmp/gemlink-deploy

# 3. On the device (sudo required for /usr/share):
ssh steamos@DEVICE_IP
sudo mkdir -p /opt/gemlink
sudo install -m 0755 /tmp/gemlink-deploy/alvr_client_openxr /opt/gemlink/
sudo install -m 0755 /tmp/gemlink-deploy/run-gemlink.sh /opt/gemlink/
sudo cp /tmp/gemlink-deploy/gemlink.desktop /usr/share/applications/

# 4. Linkage check — everything must resolve against the device's own libs:
ldd /opt/gemlink/alvr_client_openxr | grep -i "not found" || echo "deps OK"
```

## First run

**From the app grid (the real test)**: the entry inherits the compositor session's
environment; the smoke ladder should print, in order:

```
smoke[display] …
smoke[runtime] runtime '…' v…, detected platform SteamFrame
smoke[session] …
smoke[lobby] …
smoke[controller bindings] …
smoke[handshake] ConnectionAccepted sent to <PC IP>
smoke[negotiated] …
smoke[decoder-config] …
smoke[first-video-frame] …
smoke[xrEndFrame] …
```

**From SSH (debugging only)**: there is no compositor env in an SSH session, so
the client stops at the display stage by design — exit code 2. That run still
verifies linkage, the loader, and the logger. To reach further from SSH, borrow
the session environment from the running compositor process
(`/proc/$(pgrep gamescope|head -1)/environ`) or use the app-grid entry.

## The PC side

- The Windows box runs the driver built from the **same commit** as the client
  (the negotiated-config wire changed in session 19).
- The client announces on the LAN; the driver discovers it — click **Trust**.
- The device must reach the PC's IP: USB-C NCM (wired, `enablencm`) or the same
  Wi-Fi. For first sessions, wired NCM is the recommendation.

## Known-good references (from the firmware)

- Loader: `/opt/steamvr/bin/linuxarm64/libopenxr_loader.so`
- Runtime manifest: `/opt/steamvr/steamxr_linuxarm64.json` (SteamVR 2.17.10, OpenXR 1.0)
- Valve's own launcher: `/opt/steamvr/tools/vrlink/bin/linuxarm64/run_vrlink.sh`

## Toolchain reference

How the binary was built and how to rebuild it: `/workspace/tools/README-frame-build.md`
(cross toolchain, cmake-based deps, qemu verification against the firmware rootfs,
troubleshooting index). The agent-facing summary is the `frame-compile` skill.
