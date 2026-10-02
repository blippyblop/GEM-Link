# ADR-0013 — The media plane: repair, refuse, and derive the nonce

- **Status:** Accepted
- **Date:** 2026-10-02
- **Deciders:** maintainer

## Context

ADR-0011 said *never show a frame the client cannot reconstruct*, and named a signal
(a monotonic frame index) and two ends (a send side that does not transmit an undecodable
frame, a display side that does not adopt an untrustworthy picture). It did not say what the
media plane should *be*, and it deliberately left the mechanism open.

Three facts then bounded the decision:

1. **VD's transport is unrecoverable.** Six independent negative checks
   (`VD_RE/24-vd-link-qos.md` §8.1): no module in the VD payload can open a socket except
   FFmpeg, and FFmpeg's only call sites are decode-side; the installers ship no transport;
   the Android APK copy has no `classes.dex` and no assemblies. So there is nothing to port
   and no spec to conform to. What VD demonstrably has — a keyed media plane, a dedicated
   media port, a mechanism that does not lose frames silently — is a *property list*, not an
   implementation.
2. **Upstream ALVR is abandoned (ADR-0012).** Wire compatibility is no longer owed, and the
   inherited media plane is not a constraint. It is also the place the defect lives: a
   bounded channel plus `try_send` that discards a frame and breaks the decoder's reference
   chain.
3. **The mechanism is measurable offline.** `x-bench` already has deterministic impairment
   profiles with per-datagram latency, jitter, loss and stalls. A media plane can be driven
   through them with no hardware at all.

## Options considered

1. **Patch the inherited send path.** Reorder the backpressure and stop dropping. — Pros:
   smallest change; keeps the working SteamVR path untouched. Cons: leaves the receiver
   unable to tell a lost frame from a delivered one, so ADR-0011's display half stays
   unimplementable; leaves recovery at zero (a lost datagram is still a lost frame).
2. **Adopt an existing transport (RTP/QUIC/WebRTC datachannels).** — Pros: a decade of
   congestion-control and FEC work; no new protocol to debug. Cons: each imposes its own
   framing, its own handshake, and its own latency profile, and the site is a 300 Mbps
   one-way media push to a device whose kernel decodes exactly one codec. The parts we would
   take (congestion control) we are not yet ready to control against, and the parts we would
   inherit (retransmit policy, jitter buffering) are exactly the ones that have to match the
   90 Hz deadline.
3. **CHOSEN — build the media plane in `x-transport`, gate it in the bench.** A crate with
   frame packetisation, striped GF(2⁸) FEC, a pacer that never discards, per-datagram AEAD
   with derived nonces, and a receiver whose *type* cannot carry an unreconstructable frame.

## Decision

1. **Frames are packetised with a fixed-width header carrying the frame index** — ADR-0011's
   signal, and load-bearing for a second reason (see 4). The header is authenticated as AEAD
   associated data, so a relabelled frame index fails the tag.
2. **Loss is repaired, not observed.** A systematic erasure code over GF(2⁸), Cauchy-matrix
   rather than Vandermonde (a Vandermonde matrix over GF(2⁸) is not guaranteed MDS, so "any
   *k* losses" becomes "usually"). A frame is split into **interleaved blocks** of ≤ 250 data
   shards, because at the target envelope a frame is ~300 shards and 255 is the field's limit
   — and interleaving costs nothing while spreading a burst loss across every block.
3. **The receiver cannot hand out what it could not rebuild.** `DeliveredFrame::payload()`
   is `Option<&[u8]>`, `None` for an unreconstructable frame, and no bytes are constructed in
   that case. A convention ("check the flag") is what the old code forgot; a missing payload
   cannot be forgotten. The permitted response to an absent frame is to hold and reproject
   the previous one.
4. **Per-datagram AEAD with derived nonces.** Nonce = `(frame_index, fragment_index)`. A
   counter nonce desynchronises on the first lost datagram — fatal on a lossy plane and
   invisible on TCP, which is why the control plane's counter-based socket cannot be reused
   here. This also makes the cipher stateless, so there is no shared lock on the media hot
   path. The cryptographic consequence is stated in the module: nonce uniqueness rests on the
   frame index being monotonic, so a session key must not outlive a frame-index reset.
5. **Pacing backpressures; it never discards.** The order is *check deliverability → pace →
   encode → send*, because encoding before knowing whether the result can be sent is what
   made the old drop unrecoverable. The rate is an input, not a decision — a pacer that also
   adapts is two controllers fighting over one actuator.
6. **The gates are per scenario.** A link's jitter and stalls *force* a reorder window, so
   asserting one latency budget for every profile would either fail honest buffering or pass a
   buffer sized for the wrong link. Each scenario declares its own budget for latency and for
   unreconstructable frames; a scenario that cannot meet its budget must lower its bitrate,
   which is the bitrate controller's job rather than something to smuggle into the code.

## Consequences

**Easier.** The display half of ADR-0011 is now implementable, because the receiver's output
type carries the truth about whether a frame exists. Recovery is bounded and measurable
rather than hoped for. `bench transport` turns the whole thing into a table with a pass/fail
per link profile, with controls that prove the mechanisms are doing work.

**Harder.** A media plane is now ours to get right, including the parts the bench does not
model: burst loss (i.i.d. loss is kinder than reality — the block interleaving exists for
bursts and is not yet exercised by one), a real encoder's varying frame sizes, and congestion.
The striped FEC also means the sender's encode cost is proportional to `data × parity` per
frame, which is real at 27,000 datagrams per second and now has a multiply-table fast path.

**Not done here, and named rather than implied.** Nothing is wired into the live SteamVR
path: `server_core` still sends through `alvr_sockets`. The wiring step is where the
send-side half of ADR-0011 also belongs — a send path that would rather suppress frames than
transmit one whose reference it just discarded. The bench numbers are media-plane numbers,
not glass-to-glass, and no glass-to-glass instrument exists.

**Revisit if** a burst-loss model shows the block interleaving is not enough, or if a real
session shows the jitter window `jitter + stall` over the frame interval is the wrong sizing.
Both are measurable; neither should be guessed at.
