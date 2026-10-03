//! The trust gates: ADR-0011's two ends, as state machines.
//!
//! ADR-0011 says two things, and neither is optional:
//!
//! 1. **The send side never transmits a frame the client cannot reconstruct.** A discarded
//!    frame breaks the decoder's reference chain, so every following P-frame is undecodable.
//!    The response is not to drop one frame; it is to **stop transmitting** until a frame
//!    goes out that rebuilds the chain.
//! 2. **The display side never adopts an untrustworthy picture.** Hold the last good frame and
//!    reproject. Displaying a grey or otherwise unfit frame is not a permitted answer.
//!
//! ## Why this is a module and not two booleans
//!
//! Both ends already had this mechanism in the tree, as a single `stream_corrupted` flag —
//! and both were wrong in the *same* way, which is why one session produced three incorrect
//! root causes and twelve eliminations before anyone noticed that 41 % of displayed frames
//! were garbage while the telemetry read "0 errors":
//!
//! * On the **client**, the gate was armed only by *datagram* loss. A server-side discarded
//!   frame consumes no datagrams, so the transport's sequence stayed unbroken,
//!   `had_packet_loss()` stayed false, and the undecodable P-frames that followed were
//!   submitted to the decoder anyway. The frame-index gap that *would* have detected it was
//!   computed — and then thrown away into a `warn!`.
//! * On the **server**, the suppression state was gated behind a setting that defaults off,
//!   so the invariant was bypassed in the shipped configuration.
//!
//! Two instances of the same mistake, made independently in two places, is not bad luck. It
//! is what happens when a rule lives in an `if` instead of in a thing with a name and tests.
//!
//! ## The trigger, stated once
//!
//! *Trust is regained **only** by a keyframe.* Not by a run of contiguous frames, not by a
//! quiet period, not by a healthy-looking decode. A P-frame whose reference is missing decodes
//! to a perfectly plausible picture — that is exactly what made the defect silent — so "it
//! looks fine" is not evidence and is never consulted.
//!
//! ## Both gates are two-phase
//!
//! Real call sites decide *before* they know the outcome: the sender does not know whether the
//! network took the frame until it tries, and the client does not know whether the decoder
//! took the frame until it submits it. So each gate answers a question and then accepts the
//! result, rather than pretending to know both at once.

/// What the send side decided about one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendDecision {
    /// Transmit it.
    Transmit,
    /// Do not transmit it, and do not transmit anything until a keyframe goes out.
    Suppress(SuppressReason),
}

/// Why a frame may not be transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// No keyframe has been transmitted yet this session.
    NoKeyframeYet,
    /// A frame was discarded on its way to the network, so the client's reference chain is
    /// broken from that point on.
    ReferenceChainBroken,
}

/// Decides what the server may put on the wire.
///
/// Starts blocked: the first frames of a session are not decodable by a client that has not
/// seen a keyframe, and a session that begins by transmitting them begins by presenting
/// garbage.
#[derive(Debug, Clone)]
pub struct SendGate {
    /// `None` means transmitting. Anything else is why we are not.
    blocked: Option<SuppressReason>,
    discarded: u64,
    suppressed_frames: u64,
}

impl Default for SendGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SendGate {
    pub const fn new() -> Self {
        Self {
            blocked: Some(SuppressReason::NoKeyframeYet),
            discarded: 0,
            suppressed_frames: 0,
        }
    }

    /// Phase one: may this frame go?
    ///
    /// `is_keyframe` is the frame's own type, not a promise about the network — the answer can
    /// still turn out to be wrong, which is why [`SendGate::on_send_result`] exists.
    pub fn may_transmit(&mut self, is_keyframe: bool) -> SendDecision {
        if is_keyframe && self.blocked.is_some() {
            // The chain is rebuilt by *this* frame, and at this instant it is going out, so
            // resume. If the send then fails, phase two puts the block straight back.
            self.blocked = None;
        }

        match self.blocked {
            Some(reason) => {
                self.suppressed_frames += 1;
                SendDecision::Suppress(reason)
            }
            None => SendDecision::Transmit,
        }
    }

    /// Phase two: the network's answer.
    ///
    /// **A keyframe that could not be sent does not un-suppress anything.** Getting that wrong
    /// produces the loop the old code warned about in a comment — drop, request a keyframe,
    /// drop the keyframe, request again — where the sender never resumes, the client never
    /// recovers, and the two of them agree that everything is fine.
    pub fn on_send_result(&mut self, accepted: bool) {
        if !accepted {
            self.discarded += 1;
            // The reason is now the chain rather than the session's opening: the client *had*
            // a reference and it is gone. Which of the two it is changes what the log says,
            // and the log is the instrument that was missing.
            self.blocked = Some(SuppressReason::ReferenceChainBroken);
        }
    }

    /// The very first decision of a session, before any frame has been offered.
    pub fn is_suppressed(&self) -> bool {
        self.blocked.is_some()
    }

    /// Why transmission is currently stopped, if it is.
    pub fn blocked_by(&self) -> Option<SuppressReason> {
        self.blocked
    }

    /// Frames the network refused.
    pub fn discarded(&self) -> u64 {
        self.discarded
    }

    /// Frames withheld to keep the reference chain intact. This is the cost of the invariant
    /// and it is not zero: a suppressed frame is what the client sees as a hold.
    pub fn suppressed_frames(&self) -> u64 {
        self.suppressed_frames
    }
}

/// What the display side may do with one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTrust {
    /// Decode it and present it.
    Trusted,
    /// Do not present it. Hold the previous frame and reproject it.
    Untrusted {
        reason: UntrustedReason,
        /// True on the frame where trust was *lost*. The caller wants this because the right
        /// response — asking the sender for a keyframe — is a *reliable control packet*, and
        /// asking once per frame for the length of a recovery window means up to 90 requests a
        /// second, each of which the sender answers with a keyframe. That is not a recovery
        /// strategy; it is a control-plane flood with a bitrate spike attached.
        first: bool,
    },
}

/// Why a frame may not be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrustedReason {
    /// No keyframe yet this session, so no frame has a known-good reference.
    NoKeyframeYet,
    /// This frame is not contiguous with the last one, so its reference is gone. `missing` is
    /// how many frames never arrived.
    Gap { missing: u64 },
    /// Datagrams were lost inside this frame.
    DatagramLoss,
    /// The decoder refused it (saturation, or a hardware fault).
    DecoderRejected,
}

/// Decides what the client may present.
#[derive(Debug, Clone, Default)]
pub struct TrustGate {
    last_frame_index: Option<u64>,
    /// `None` means trusted. Anything else is why we are not.
    blocked: Option<UntrustedReason>,
    missed_frames: u64,
    untrusted_frames: u64,
    /// Consecutive untrusted frames, reset the moment a frame is trusted. This is what makes
    /// "we just lost trust" distinguishable from "we have been untrusted for a while", which
    /// is the difference between announcing a loss and re-announcing it forever.
    untrusted_run: u64,
}

impl TrustGate {
    pub fn new() -> Self {
        Self {
            last_frame_index: None,
            blocked: Some(UntrustedReason::NoKeyframeYet),
            missed_frames: 0,
            untrusted_frames: 0,
            untrusted_run: 0,
        }
    }

    /// Phase one: may this frame be presented?
    ///
    /// The frame index is ADR-0011's signal and it is the *only* thing that detects a
    /// server-side discard: a discarded frame consumes no datagrams, so nothing at the
    /// transport layer moves.
    pub fn may_present(
        &mut self,
        frame_index: u64,
        is_keyframe: bool,
        had_datagram_loss: bool,
    ) -> FrameTrust {
        let gap = match self.last_frame_index {
            // A *forward* jump is a hole. Anything else — an equal index, or one behind — is a
            // duplicate or a reorder, which are not holes and must not be counted as losses,
            // or the reported figure inflates on every retransmit.
            Some(last) if frame_index > last.saturating_add(1) => {
                let missing = frame_index - last - 1;
                self.missed_frames += missing;
                Some(missing)
            }
            _ => None,
        };
        if self.last_frame_index.is_none_or(|last| frame_index > last) {
            self.last_frame_index = Some(frame_index);
        }

        // Each clause is a distinct way for a picture to be unfit. A keyframe repairs the
        // chain, so it is checked before the gap — but a keyframe with a hole in it is not a
        // keyframe, so datagram loss is checked first.
        if had_datagram_loss {
            self.blocked = Some(UntrustedReason::DatagramLoss);
        } else if let Some(missing) = gap {
            if is_keyframe {
                self.blocked = None;
            } else {
                self.blocked = Some(UntrustedReason::Gap { missing });
            }
        } else if is_keyframe {
            self.blocked = None;
        }
        // Otherwise: contiguous, intact, and not a keyframe. Whatever trust we had stands. A
        // run of plausible-looking P-frames after a broken reference is exactly the failure
        // this gate exists to catch, so a clean decode is not evidence of anything.

        match self.blocked {
            None => {
                self.untrusted_run = 0;
                FrameTrust::Trusted
            }
            Some(reason) => {
                self.untrusted_run += 1;
                self.untrusted_frames += 1;
                // `first` is the *transition* into untrusted, not the first time in the
                // session. It used to be `untrusted_frames == 0`, which is true exactly once
                // ever — so the second time the chain broke, the client held in silence and
                // never asked for the keyframe that would release it. On hardware that was
                // 84 frames decoded and 5,000 held: a permanently black screen.
                let first = self.untrusted_run == 1;
                FrameTrust::Untrusted { reason, first }
            }
        }
    }

    /// Should the client spend a control packet asking for a keyframe?
    ///
    /// Consecutive untrusted frames so far. Reset the moment a frame is trusted.
    ///
    /// Exposed because *when to ask again* is a timing question, not a trust question, and it
    /// belongs with a clock. This gate used to answer it with a frame count
    /// (`KEYFRAME_RETRY_FRAMES`), which made the re-ask interval depend on the frame rate — a
    /// different real interval at 30 Hz than at 90 Hz — and left the ladder written down twice,
    /// here and in the decoder. `client_core::stall::StuckDetector` is now the only ladder, and
    /// this counter is what it needs to see that a hold began and that a recovery ended.
    pub fn untrusted_run(&self) -> u64 {
        self.untrusted_run
    }

    /// Phase two: the decoder's answer.
    ///
    /// A frame the decoder refused is not a frame we have, so trust is revoked until a
    /// keyframe — the same rule, for the same reason.
    pub fn on_decoder_result(&mut self, accepted: bool) {
        if !accepted {
            self.blocked = Some(UntrustedReason::DecoderRejected);
        }
    }

    pub fn is_trusted(&self) -> bool {
        self.blocked.is_none()
    }

    /// Why the picture is currently untrusted, if it is.
    pub fn blocked_by(&self) -> Option<UntrustedReason> {
        self.blocked
    }

    /// Frames the sender discarded, as seen from this end. **This is the number that was
    /// missing**: the old code computed it and logged it, and nothing counted "a frame I could
    /// not present", so 41 % of frames could be garbage while every counter read zero.
    pub fn missed_frames(&self) -> u64 {
        self.missed_frames
    }

    /// Frames held rather than presented.
    pub fn untrusted_frames(&self) -> u64 {
        self.untrusted_frames
    }

    pub fn last_frame_index(&self) -> Option<u64> {
        self.last_frame_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the send gate the way the send path does: ask, then report.
    fn send(gate: &mut SendGate, is_keyframe: bool, accepted: bool) -> SendDecision {
        let decision = gate.may_transmit(is_keyframe);
        if decision == SendDecision::Transmit {
            gate.on_send_result(accepted);
        }
        decision
    }

    /// Drive the display gate the way the client does: ask, then report the decode.
    fn present(gate: &mut TrustGate, index: u64, is_keyframe: bool, loss: bool) -> FrameTrust {
        let trust = gate.may_present(index, is_keyframe, loss);
        if trust == FrameTrust::Trusted {
            gate.on_decoder_result(true);
        }
        trust
    }

    fn reason(trust: FrameTrust) -> UntrustedReason {
        match trust {
            FrameTrust::Untrusted { reason, .. } => reason,
            FrameTrust::Trusted => panic!("expected untrusted"),
        }
    }

    fn first(trust: FrameTrust) -> bool {
        match trust {
            FrameTrust::Untrusted { first, .. } => first,
            FrameTrust::Trusted => panic!("expected untrusted"),
        }
    }

    // -- send side ----------------------------------------------------------

    #[test]
    fn a_session_does_not_open_by_transmitting_p_frames() {
        // The first frames of a session are undecodable to a client that has not seen a
        // keyframe, so a session that transmits them opens by presenting garbage.
        let mut gate = SendGate::new();
        assert!(gate.is_suppressed());
        assert_eq!(
            gate.may_transmit(false),
            SendDecision::Suppress(SuppressReason::NoKeyframeYet)
        );
        assert_eq!(send(&mut gate, true, true), SendDecision::Transmit);
        assert!(!gate.is_suppressed());
    }

    #[test]
    fn a_discarded_frame_stops_transmission_until_a_keyframe_goes_out() {
        let mut gate = SendGate::new();
        assert_eq!(send(&mut gate, true, true), SendDecision::Transmit);

        // The network refuses frame 2.
        assert_eq!(send(&mut gate, false, false), SendDecision::Transmit);
        assert!(gate.is_suppressed(), "a refused frame must stop the stream");
        // Frames 3 and 4 are undecodable without 2, so they are not even offered.
        assert_eq!(
            gate.may_transmit(false),
            SendDecision::Suppress(SuppressReason::ReferenceChainBroken)
        );
        assert!(matches!(
            gate.may_transmit(false),
            SendDecision::Suppress(_)
        ));
        assert_eq!(gate.discarded(), 1);

        // The keyframe arrives and goes out, rebuilding the chain.
        assert_eq!(send(&mut gate, true, true), SendDecision::Transmit);
        assert_eq!(send(&mut gate, false, true), SendDecision::Transmit);
        assert_eq!(gate.suppressed_frames(), 2);
    }

    #[test]
    fn a_keyframe_that_could_not_be_sent_does_not_resume_anything() {
        // The loop the old comment warned about: drop, ask for a keyframe, drop the keyframe,
        // ask again — where the sender never resumes and both ends agree that all is well.
        let mut gate = SendGate::new();
        assert_eq!(send(&mut gate, true, true), SendDecision::Transmit);
        assert_eq!(send(&mut gate, false, false), SendDecision::Transmit);
        assert!(gate.is_suppressed());

        // The requested keyframe is itself discarded. Phase one cleared the block; phase two
        // must put it straight back.
        assert_eq!(send(&mut gate, true, false), SendDecision::Transmit);
        assert!(
            gate.is_suppressed(),
            "a lost keyframe must not leave the stream resumed"
        );

        // Only a keyframe that actually goes out resumes it.
        assert_eq!(send(&mut gate, true, true), SendDecision::Transmit);
        assert!(!gate.is_suppressed());
    }

    #[test]
    fn contiguous_frames_are_transmitted_and_nothing_is_suppressed() {
        let mut gate = SendGate::new();
        assert_eq!(send(&mut gate, true, true), SendDecision::Transmit);
        for _ in 0..100 {
            assert_eq!(send(&mut gate, false, true), SendDecision::Transmit);
        }
        assert_eq!(gate.discarded(), 0);
        assert_eq!(gate.suppressed_frames(), 0);
    }

    // -- display side -------------------------------------------------------

    #[test]
    fn the_first_frame_must_be_a_keyframe_to_be_trusted() {
        let mut gate = TrustGate::new();
        assert_eq!(
            reason(present(&mut gate, 1, false, false)),
            UntrustedReason::NoKeyframeYet
        );
        assert_eq!(present(&mut gate, 2, true, false), FrameTrust::Trusted);
        assert!(gate.is_trusted());
    }

    #[test]
    fn a_gap_makes_every_following_frame_untrustworthy_until_a_keyframe() {
        // The defect, as a test. The server discarded frame 3. Frames 4 and 5 arrive perfectly
        // intact — every datagram present, the decoder happy — and they are garbage, because
        // their reference is gone. Nothing at the transport layer moved: a discarded frame
        // consumes no datagrams.
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        assert_eq!(present(&mut gate, 2, false, false), FrameTrust::Trusted);

        // 3 never arrives.
        assert_eq!(
            reason(present(&mut gate, 4, false, false)),
            UntrustedReason::Gap { missing: 1 }
        );
        assert_eq!(gate.missed_frames(), 1);
        // A clean decode is not evidence.
        assert!(matches!(
            present(&mut gate, 5, false, false),
            FrameTrust::Untrusted { .. }
        ));
        assert!(matches!(
            present(&mut gate, 6, false, false),
            FrameTrust::Untrusted { .. }
        ));

        // The keyframe rebuilds the chain, and only then.
        assert_eq!(present(&mut gate, 7, true, false), FrameTrust::Trusted);
        assert_eq!(present(&mut gate, 8, false, false), FrameTrust::Trusted);
        assert_eq!(gate.untrusted_frames(), 3);
    }

    #[test]
    fn the_keyframe_is_asked_for_once_not_once_per_frame() {
        // During a recovery window the client is untrusted for frame after frame. Asking the
        // sender for a keyframe on each of them is up to 90 reliable control packets a second,
        // each answered with a keyframe: a flood with a bitrate spike attached.
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        assert!(
            first(present(&mut gate, 5, false, false)),
            "the loss is announced once"
        );
        for index in 6..20 {
            assert!(
                !first(present(&mut gate, index, false, false)),
                "frame {index} asked for another keyframe"
            );
        }
        // And the counter is right about what was lost: one gap, not fourteen.
        assert_eq!(gate.missed_frames(), 3);
    }

    #[test]
    fn a_second_gap_asks_for_a_keyframe_again() {
        // The bug that shipped to hardware. `first` was `untrusted_frames == 0`, which is true
        // once per *session*: the first recovery worked, and the second time the chain broke
        // the client held in silence. 84 frames decoded, 5,000 held, black screen.
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);

        // First loss, announced.
        assert!(first(present(&mut gate, 5, false, false)));

        // Recovered.
        assert_eq!(present(&mut gate, 9, true, false), FrameTrust::Trusted);
        assert_eq!(present(&mut gate, 10, false, false), FrameTrust::Trusted);

        // Second loss: it must ask again.
        assert!(
            first(present(&mut gate, 14, false, false)),
            "a second gap did not ask for a keyframe"
        );
    }

    #[test]
    fn the_untrusted_run_is_visible_so_a_clock_can_watch_it() {
        // The gate reports the state; it no longer decides the timing. A caller that holds a
        // `StuckDetector` needs exactly two facts — that a hold began, and that one ended — and
        // this is both, because the count resets on recovery.
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        assert_eq!(gate.untrusted_run(), 0);

        // 3 never arrives.
        assert!(matches!(
            present(&mut gate, 4, false, false),
            FrameTrust::Untrusted { .. }
        ));
        for index in 5..12 {
            let _ = present(&mut gate, index, false, false);
        }
        assert_eq!(gate.untrusted_run(), 8);

        // The keyframe ends the hold, and the run with it.
        assert_eq!(present(&mut gate, 12, true, false), FrameTrust::Trusted);
        assert_eq!(
            gate.untrusted_run(),
            0,
            "the run did not reset, so a clock would never re-arm"
        );
    }
    #[test]
    fn a_keyframe_that_arrives_damaged_does_not_restore_trust() {
        // A keyframe with a hole in it is not a keyframe.
        let mut gate = TrustGate::new();
        assert!(matches!(
            present(&mut gate, 1, false, false),
            FrameTrust::Untrusted { .. }
        ));
        assert_eq!(
            reason(present(&mut gate, 2, true, true)),
            UntrustedReason::DatagramLoss
        );
        assert!(!gate.is_trusted());
        assert_eq!(present(&mut gate, 3, true, false), FrameTrust::Trusted);
    }

    #[test]
    fn a_decoder_rejection_makes_the_next_keyframe_required() {
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        // The decoder refuses frame 2.
        assert_eq!(gate.may_present(2, false, false), FrameTrust::Trusted);
        gate.on_decoder_result(false);
        assert!(!gate.is_trusted());
        assert_eq!(
            reason(present(&mut gate, 3, false, false)),
            UntrustedReason::DecoderRejected
        );
        assert_eq!(present(&mut gate, 4, true, false), FrameTrust::Trusted);
    }

    #[test]
    fn duplicates_and_reorders_are_not_gaps() {
        // A retransmit or a reordered frame must not inflate the loss figure, or the number
        // reported to the user drifts upward on every duplicate.
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        assert_eq!(present(&mut gate, 2, false, false), FrameTrust::Trusted);
        // 2 again, then 1 again: neither is a hole.
        assert_eq!(present(&mut gate, 2, false, false), FrameTrust::Trusted);
        assert_eq!(present(&mut gate, 1, false, false), FrameTrust::Trusted);
        assert_eq!(gate.missed_frames(), 0);
        assert_eq!(gate.last_frame_index(), Some(2));
        assert!(gate.is_trusted());
    }

    #[test]
    fn a_large_gap_is_counted_exactly_once() {
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        assert_eq!(
            reason(present(&mut gate, 20, false, false)),
            UntrustedReason::Gap { missing: 18 }
        );
        assert_eq!(gate.missed_frames(), 18);
        // And the frames that follow report the same single loss, not a new one.
        assert!(matches!(
            present(&mut gate, 21, false, false),
            FrameTrust::Untrusted { .. }
        ));
        assert_eq!(gate.missed_frames(), 18);
    }

    #[test]
    fn a_frame_index_of_zero_does_not_break_the_gap_arithmetic() {
        // The session-9 caution: a frame index pinned at zero would look contiguous to a gap
        // check. It still must not overflow or wrap, and it must not be trusted.
        let mut gate = TrustGate::new();
        assert!(matches!(
            present(&mut gate, 0, false, false),
            FrameTrust::Untrusted { .. }
        ));
        assert!(matches!(
            present(&mut gate, 0, false, false),
            FrameTrust::Untrusted { .. }
        ));
        assert_eq!(gate.missed_frames(), 0);
        assert_eq!(present(&mut gate, 0, true, false), FrameTrust::Trusted);
    }

    #[test]
    fn the_two_gates_agree_about_a_lost_keyframe() {
        // One timeline, both ends: the server discards the frame it had just requested a
        // keyframe for. The client must not trust anything until a keyframe actually arrives.
        let mut send_gate = SendGate::new();
        let mut trust = TrustGate::new();

        // Frame 1: the opening keyframe, transmitted and trusted.
        assert_eq!(send(&mut send_gate, true, true), SendDecision::Transmit);
        assert_eq!(present(&mut trust, 1, true, false), FrameTrust::Trusted);

        // Frame 2 is discarded on the way out.
        assert_eq!(send(&mut send_gate, false, false), SendDecision::Transmit);
        assert!(send_gate.is_suppressed());

        // Frame 3 is the keyframe the server asked for, and it is discarded too.
        assert_eq!(send(&mut send_gate, true, false), SendDecision::Transmit);
        assert!(send_gate.is_suppressed());

        // Frame 4 is not a keyframe, so it is not transmitted either.
        assert!(matches!(
            send_gate.may_transmit(false),
            SendDecision::Suppress(_)
        ));
        assert!(send_gate.is_suppressed());

        // The client saw 1, then nothing, then 5. It holds.
        assert!(matches!(
            present(&mut trust, 5, false, false),
            FrameTrust::Untrusted { .. }
        ));
        assert!(!trust.is_trusted());
        assert_eq!(trust.missed_frames(), 3);
    }
}
