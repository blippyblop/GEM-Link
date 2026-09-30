# SECURITY POLICY

## Supported versions
Latest release and `main`. We ship fast; upgrade.

## Reporting
Report via **GitHub private vulnerability reporting** (no project domain yet; email channel comes with the first release). Please do not open public issues for vulnerabilities. Response target: 72 hours acknowledgment, 30 days fix or mitigation.

## Scope
- The Windows server, drivers (IDD/audio), the headset client, the transport protocol, and the pairing/trust model.
- Out of scope: SteamVR/Monado internals, GPU driver bugs (we'll help route), physical attacks on devices.

## Security stance (by design)
- **Transport encryption is always on** (Noise-XX handshake at pairing → AEAD per-frame); there is no plaintext mode.
- **No telemetry.** Nothing leaves the machine except the stream to the headset you paired.
- Pairing is explicit and mutual; unknown clients require physical confirmation on the PC.
- Drivers (IDD/audio) are the highest-risk surface: signed builds only, least privilege, and a documented rollback path.
- The desktop-streaming path treats protected-content detection as a hard stop (frames are never captured or transmitted).
