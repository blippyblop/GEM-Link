# ADR-0011 — Never show a frame the client cannot trust

- **Status:** Accepted
- **Date:** 2026-10-02
- **Deciders:** maintainer

## Context

GemLink spent a session chasing "grey frames" that turned out to be a single
mechanism, measured end to end on the emulated rig
(`/workspace/VD_RE/50-grey-frame-experiments.md`):

- The server's video send path uses a **bounded channel** and `try_send`
  (`server_core/src/lib.rs`). When the client cannot keep up, the encoded frame is
  **dropped on the floor** and an IDR is requested. 12.8 % of frames were dropped
  in a 1500-frame run.
- A dropped frame breaks the HEVC reference chain, so **the following P-frames
  cannot be reconstructed**. Both libde265 and an independent ffmpeg 7.0.2 decode
  the same captured bitstream to a **flat mid-grey** (mean 128, tile σ ≈ 2) —
  ffmpeg reporting `Could not find ref with POC n` as it does.
- The damage heals only when the requested IDR arrives: measured medians of
  **3 frames** poisoned per drop, which is why the greys appeared in runs of ~3.
- Correlation: with 0 frames skipped, 35 % grey; with **1** frame skipped, **81 %**
  grey.

Three things about this are the real lesson.

**1. It is not an edge case.** Source frame rate will fall below the display rate
for short periods on real hardware — a loading screen, a hitch, a heavy scene.
12.8 % loss *from a mild throughput mismatch* is not a corner; it is the normal
operating envelope. A design that requires every frame to arrive is a design that
greys the user's screen whenever the world gets busy.

**2. The failure was invisible to us.** The client's telemetry reported
**"0 errors" and "0 skipped"** while 41 % of the frames it displayed were
reconstruction garbage. `report_frame_decoded` reports that a picture came out of
the decoder, not that the picture is *usable*. A defect that cannot be seen from
the telemetry is a defect that will be shipped, and it cost a session that should
have taken an hour. **Observability is part of the correctness of this feature,
not a nicety on top of it.**

**3. The receiver was left to infer something the sender already knew.** The
server knows exactly when it drops a frame — it is in the `Err(TrySendError::Full)`
arm — and it throws that knowledge away. The client's only recourse is to infer a
gap from a timestamp delta, in a codebase where the timestamp that labels a frame
is itself a **pose-history match** (87 % duplicated) read racily by the encoder
thread. That is not a channel you can build a correctness guarantee on.

## Options considered

1. **Client-side heuristic: detect a timestamp gap, hold the previous frame.**
   Cheapest to write. Rejected as the *mechanism*: it infers loss from a signal we
   have measured to be unreliable, and it cannot distinguish "a frame was dropped"
   from "the timestamp is a duplicate" — which is the *majority* case here. It also
   leaves the poisoned frames arriving and being decoded, and only discards them
   late. Acceptable as a *belt*, never as the *braces*.
2. **Rely on the decoder to report unreconstructable pictures.** ffmpeg does
   (`Could not find ref with POC n`). Whether libde265 — and, critically, the
   shipped path's V4L2 `iris` hardware decoder — can, is unknown and not ours to
   guarantee. Rejected as the *mechanism*: a correctness invariant must not depend
   on a third party's diagnostic richness, and the shipped decoder is hardware.
   Worth noting for the telemetry half: the libde265 build we ship **does** export
   `de265_get_warning`, and the harness **never calls it** — so part of "we could
   not see this" is simply that we were not asking. Fixing that is required by
   decision §4 but is not load-bearing for the invariant.
3. **Never drop: pace the server to what the client can decode** (backpressure
   instead of `try_send`). Right and necessary, but insufficient on its own: a real
   wireless link can still lose packets, and a real source can still miss frames.
   Keeps the invariant only on a perfect link.
4. **Server-side: never transmit a frame the client cannot reconstruct; client-side:
   never adopt a picture the decoder could not reconstruct; both ends count it, and
   the transport carries an explicit monotonic frame index so loss is *known*, not
   inferred.** Chosen.

## Decision

**A frame that cannot be reconstructed must never reach the display.** This is an
invariant with two ends and one signal.

1. **Send side — never send an untrustworthy frame.** If the server drops or
   suppresses a frame, it must **suppress every following frame until the next
   IDR is actually emitted**, not just the dropped one. Today one drop poisons the
   next ~3 frames with P-frames the client cannot decode; the correct behaviour is
   one clean, bounded, *knowable* gap. "Several subtly broken frames" is strictly
   worse than "one honest hole".
2. **Display side — never adopt an untrustworthy picture.** The client updates its
   "current frame" only when the picture is trustworthy. Otherwise it **keeps
   displaying the last good frame, reprojected to the current pose**, until a good
   frame arrives. No grey, no jump, no sudden change: the user sees the world hold
   still and stay head-locked, which is what reprojection is for. This is the
   client-side half of the rule the user stated as *"it should be redisplaying the
   previous frame at the very worst case"*, and it is also the substrate for
   VD-style synthesised frames later (BAR.md, `11-vd-features-missing-from-alvr.md`
   row 1) — synthesis replaces the hold, it does not replace the invariant.
3. **The signal is explicit, and it comes from the sender.** The video header
   carries a **monotonic frame index** so a gap is unambiguous and cannot be
   confused with a duplicated or stale timestamp. The sender knows when it dropped;
   it says so. The receiver must not be asked to guess.
4. **Both ends count it, and the bench asserts on it.** Server-side "frames
   suppressed"; client-side "frames held" and — the one that matters —
   **"frames displayed that could not be reconstructed", which must be zero.**
   A decode that yields an unusable picture is a **corruption event, not a
   success**: the current telemetry calling 41 % garbage "0 errors" is a defect in
   its own right and is fixed under this ADR.

## Consequences

**Easier.** The user-visible failure mode becomes "the image holds and stays
head-locked for a few frames" instead of "the screen goes grey" — at *any* source
frame rate, which is the whole point. Latency tails improve rather than degrade
under load, because a held frame is reprojected, not re-sent. The invariant is
testable without hardware: induce loss in the bench and assert zero bogus
displays.

**Harder / accepted costs.** The client gains a trust gate, which means it needs a
notion of "trustworthy" that does not depend on the decoder (per Option 2) — hence
the explicit index. The client must hold and reproject, which means an honest
reprojection path is now on the critical path rather than a later polish item. The
server must be able to suppress a run of frames, i.e. the send path grows a state
("we are waiting for an IDR") that it does not have today.

**Revisit when:** frame synthesis (SSW-class) lands — at that point the *hold*
becomes a *synthesise*, and only the synthesis quality is up for debate; the
invariant does not change. Also revisit if the shipped `iris` decoder turns out to
report unreconstructable pictures reliably, in which case the client's trust gate
can be *strengthened* with decoder input — never replaced by it.

## Relation to existing gates

ROADMAP M4 (`v0.5.0`) already gates "**zero dropped frames on the impairment
profiles**". That is a *transport* gate and it is necessary but not sufficient: it
holds only while the link behaves. This ADR adds the **display** gate, earlier and
independently: drops may happen; a bogus frame may never be shown.

## Implementation status (2026-10-02)

**In force on both ends.** The invariant is implemented as `x_transport::{SendGate, TrustGate}` —
two state machines with tests — and wired into the two call sites that matter:
`server_core::send_video_nal` (send half) and `client_core`'s video receive loop (display half).

What that replaced is worth recording, because it explains a whole session. **Both ends already
had this mechanism**, as a single `stream_corrupted` boolean, and both were wrong in the same
two ways:

1. **The display gate was armed on the wrong signal.** It fired on *datagram* loss. A frame the
   server discarded consumes no datagrams, so the transport's sequence stayed unbroken,
   `had_packet_loss()` stayed false, and the undecodable P-frames that followed were submitted
   to the decoder. The frame-index gap that *would* have detected it was computed — and then
   thrown away into a `warn!`. That is the "wrong trigger" note in the handoff, and it was
   three characters of missing code.
2. **Both gates were bypassed by a setting, and the setting defaulted off.** `avoid_video_glitching`
   gated the *invariant itself*, so the shipped configuration transmitted and displayed frames
   that could not be reconstructed. A settings toggle that disables an invariant is not a
   configuration, it is the defect.

Three consequences, all deliberate:

- **`avoid_video_glitching` no longer disables anything.** It now controls the one genuinely
  optional part of the response: whether the client spends a reliable control packet and an
  encoder keyframe asking to *shorten* the hold. It defaults on. Its help text was rewritten
  because the old text described the invariant, which is no longer what it does.
- **A keyframe request is made once per recovery, not once per frame.** The old client asked on
  every untrusted frame — up to 90 reliable control packets a second, each answered by the
  sender with a keyframe. That is a control-plane flood with a bitrate spike attached, and it
  would have been reached constantly once the gap check started arming the gate.
- **A keyframe that arrived with datagrams missing does not restore trust.** A keyframe with a
  hole in it is not a keyframe.

**Not yet done, and it is the half that matters at scale:** the *recovery* half. The send gate
stops poisoning the stream; it does not stop the loss that caused it. Repairing the lost
datagram rather than suppressing around it is the media plane
([ADR-0013](ADR-0013-media-plane.md)), which is built and bench-gated and **not wired into this
path**. Until it is, a loss costs a hold — bounded and honest, but a hold.

**Verification.** The send-half state machine and the display-half state machine are unit-tested
(13 tests, including the two-ends-agree timeline). Both call sites type-check and lint clean for
`x86_64-pc-windows-gnu`. **Neither has been exercised on hardware** — `.36` is offline — so the
first action when it returns is still a capture with `RUST_LOG=info` to see the gating, the hold
counts and the keyframe requests in the log.
