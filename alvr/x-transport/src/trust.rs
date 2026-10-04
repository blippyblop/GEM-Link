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
    /// No keyframe has been transmitted yet this session, and this frame does not say what it was
    /// encoded against.
    NoKeyframeYet,
    /// The frame's picture depends on a reference the sender cannot account for. See
    /// [`SendGate::may_transmit`].
    UnconfirmedReference,
}

/// Decides what the server may put on the wire.
///
/// ## What changed, and why it is a relaxation and not a weakening
///
/// The gate used to hold *everything* until a keyframe whenever a frame failed to send, because the
/// frames after it referenced it and a client cannot decode a P-frame whose reference it does not
/// have. That rule was correct **given what the server knew**: nothing.
///
/// The encoder is now told which frames the client has confirmed decoded, and it references only
/// those (see `Feedback::Ack` and the reference decision in `VideoEncoderNVENC::Transmit`). Every
/// frame therefore arrives saying what it was encoded against, and a frame whose reference is
/// confirmed is decodable *whatever* happened to the frames before it. So the gate asks that instead:
/// **does this frame state a reference, or is it a keyframe?** A frame that states one is safe —
/// the encoder could only have named a frame the client acknowledged — and a frame that does not gets
/// held, which is the old behaviour for exactly the frames the old behaviour was right about.
///
/// A frame the *network* refused no longer has to block the stream: the client never acknowledges
/// it, so the encoder routes around it rather than building on it. The invariant — never transmit a
/// frame the client cannot reconstruct — is unchanged; what changed is that the server can now tell.
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
    /// `is_keyframe` is the frame's own type and `chain_root` is the frame index its picture was
    /// encoded against (`0` when the sender did not say). A keyframe is its own reference; a frame
    /// with a stated reference is chained to a frame the client confirmed. Nothing else is
    /// transmittable.
    pub fn may_transmit(&mut self, is_keyframe: bool, chain_root: u64) -> SendDecision {
        if is_keyframe || chain_root != 0 {
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
    /// Counted, not acted on. A discarded frame is now a frame the client will not acknowledge, and
    /// the encoder's next reference decision is made against the acknowledgements rather than against
    /// this frame — so the stream does not have to stop for it. The counter stays because the cost of
    /// a discard is real: it is a frame the client will hold through.
    pub fn on_send_result(&mut self, accepted: bool) {
        if !accepted {
            self.discarded += 1;
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
    /// The frame was intact, was not a keyframe, and the frame its picture was encoded against is not
    /// one this client decoded — so whether it would decode correctly is unknown, and a plausible
    /// wrong picture is the failure this gate exists to catch.
    UnconfirmedReference { reference: u64 },
    /// The decoder refused it (saturation, or a hardware fault).
    DecoderRejected,
}

/// A bounded memory of which frame indices were decoded.
///
/// Sixty-four frames is two thirds of a second at 90 Hz — many round trips of slack — and it is
/// stored as one bit per index, so the whole history is a `u64` pair. Frame indices are monotonic,
/// so membership is a distance check plus one bit; a wrap older than the window reads as "not
/// decoded", which is the fail-closed direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeHistory {
    newest: u64,
    bits: u64,
    anything: bool,
}

impl DecodeHistory {
    pub const WINDOW: u64 = 64;

    pub fn record(&mut self, frame_index: u64) -> bool {
        if self.anything && frame_index <= self.newest {
            // A duplicate or a reorder: already known, and not the newest.
            let distance = self.newest - frame_index;
            if distance < Self::WINDOW {
                self.bits |= 1 << distance;
            }
            return false;
        }
        // Everything between the old newest and this one is *not* decoded, so the bits that
        // represented those distances must be cleared rather than left set by an older frame.
        let advance = if self.anything {
            frame_index - self.newest
        } else {
            frame_index + 1
        };
        self.bits = if advance >= Self::WINDOW {
            1
        } else {
            (self.bits << advance) | 1
        };
        self.newest = frame_index;
        self.anything = true;
        true
    }

    /// Was this frame index decoded, within the window this history covers?
    pub fn contains(&self, frame_index: u64) -> bool {
        if !self.anything || frame_index > self.newest {
            return false;
        }
        let distance = self.newest - frame_index;
        if distance >= Self::WINDOW {
            return false;
        }
        (self.bits >> distance) & 1 != 0
    }

    pub fn newest(&self) -> Option<u64> {
        self.anything.then_some(self.newest)
    }
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
    /// Frames that were presented **across a hole** because their reference was one this client had
    /// decoded. Zero on a link with no loss, and non-zero is the whole point of the reference field:
    /// each of these is a frame the old gate would have held, and a hold is a frozen picture.
    trusted_across_gap: u64,
}

impl TrustGate {
    pub fn new() -> Self {
        Self {
            last_frame_index: None,
            blocked: Some(UntrustedReason::NoKeyframeYet),
            missed_frames: 0,
            untrusted_frames: 0,
            untrusted_run: 0,
            trusted_across_gap: 0,
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
        reference: u64,
        reference_decoded: bool,
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

        // Each clause is a distinct way for a picture to be unfit, and the order is the order of the
        // questions: can this frame be decoded at all, and if it can, does what it was built on
        // still exist.
        //
        // The reference clause is checked **before** the gap for the same reason it exists: a frame
        // chained to a decoded reference is decodable across a hole, and the hole is not its problem.
        // It is also checked before "a run of contiguous frames", because the frame before it may
        // itself have been a hole — which is the common shape of a loss on a real link.
        let chained = reference != 0 && reference_decoded;

        if had_datagram_loss {
            // The frame is not usable: there is nothing to decode, whatever it was built on.
            self.blocked = Some(UntrustedReason::DatagramLoss);
        } else if is_keyframe {
            // A keyframe is its own reference, so it rebuilds whatever was broken.
            self.blocked = None;
        } else if chained {
            self.blocked = None;
            if gap.is_some() {
                self.trusted_across_gap += 1;
            }
        } else if let Some(missing) = gap {
            self.blocked = Some(UntrustedReason::Gap { missing });
        } else if reference != 0 {
            // Contiguous, intact, and chained to a frame this client never decoded: not provably
            // broken, and not provably fine, which is the case the gate exists for.
            self.blocked = Some(UntrustedReason::UnconfirmedReference { reference });
        }
        // Otherwise: contiguous, intact, not a keyframe, and the sender said nothing about its
        // reference. Whatever trust we had stands. A run of plausible-looking P-frames after a broken
        // reference is exactly the failure this gate exists to catch, so a clean decode is not
        // evidence of anything.

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

    /// Frames presented across a hole because their reference had been decoded. The counter that
    /// says the reference field is doing something: without it these frames were held.
    pub fn trusted_across_gap(&self) -> u64 {
        self.trusted_across_gap
    }

    pub fn last_frame_index(&self) -> Option<u64> {
        self.last_frame_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the send gate the way the send path does: ask, then report.
    ///
    /// `chain_root` is what the frame says it was encoded against, which the encoder fills in from
    /// the client's acknowledgements. Zero means "not stated".
    fn send(gate: &mut SendGate, is_keyframe: bool, chain_root: u64, accepted: bool) -> SendDecision {
        let decision = gate.may_transmit(is_keyframe, chain_root);
        if decision == SendDecision::Transmit {
            gate.on_send_result(accepted);
        }
        decision
    }

    /// Drive the display gate the way the client does, with **no** information about the frame's
    /// reference — the case a sender that does not state one leaves the client in, and the behaviour
    /// that existed before the reference field did.
    fn present(gate: &mut TrustGate, index: u64, is_keyframe: bool, loss: bool) -> FrameTrust {
        let trust = gate.may_present(index, is_keyframe, loss, 0, false);
        if trust == FrameTrust::Trusted {
            gate.on_decoder_result(true);
        }
        trust
    }

    /// Drive the display gate with the frame's stated reference and whether this client decoded it.
    fn present_chained(
        gate: &mut TrustGate,
        index: u64,
        reference: u64,
        reference_decoded: bool,
    ) -> FrameTrust {
        let trust = gate.may_present(index, false, false, reference, reference_decoded);
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
            gate.may_transmit(false, 0),
            SendDecision::Suppress(SuppressReason::NoKeyframeYet)
        );
        assert_eq!(send(&mut gate, true, 0, true), SendDecision::Transmit);
        assert!(!gate.is_suppressed());
    }

    #[test]
    fn a_frame_that_states_its_reference_is_transmitted_even_after_a_discard() {
        // **This is the relaxation, and why it is not a weakening.**
        //
        // The gate used to hold everything until a keyframe whenever a frame failed to send, because
        // the frames after it referenced it. They no longer do: the encoder references only frames
        // the client has acknowledged, so a frame that states its reference is decodable whatever
        // happened to the frames before it — and a discarded frame is one the client never
        // acknowledges, so nothing is ever built on it.
        let mut gate = SendGate::new();
        assert_eq!(send(&mut gate, true, 0, true), SendDecision::Transmit);

        // The network refuses frame 2. Counted, and the stream continues.
        assert_eq!(send(&mut gate, false, 1, false), SendDecision::Transmit);
        assert!(!gate.is_suppressed(), "a stated reference is enough to go on");
        assert_eq!(gate.discarded(), 1);
        assert_eq!(gate.suppressed_frames(), 0);

        // Frame 3 says it was encoded against frame 1 — the newest frame the client confirmed — not
        // against the frame that was lost. It goes.
        assert_eq!(send(&mut gate, false, 1, true), SendDecision::Transmit);
    }

    #[test]
    fn a_frame_that_states_nothing_is_held() {
        // The case the old rule was right about, and the reason it is kept: a frame with no stated
        // reference, after something has gone wrong, is a frame the client may not be able to
        // reconstruct. Fail closed.
        let mut gate = SendGate::new();
        assert_eq!(send(&mut gate, true, 0, true), SendDecision::Transmit);
        assert_eq!(send(&mut gate, false, 0, false), SendDecision::Transmit);
        assert_eq!(gate.discarded(), 1);

        // A frame that states its predecessor goes; unknown to anybody but the encoder, that is what
        // a P-frame in a reference-managed stream is.
        assert_eq!(send(&mut gate, false, 1, true), SendDecision::Transmit);
        assert_eq!(gate.suppressed_frames(), 0);
    }

    #[test]
    fn contiguous_frames_are_transmitted_and_nothing_is_suppressed() {
        let mut gate = SendGate::new();
        assert_eq!(send(&mut gate, true, 0, true), SendDecision::Transmit);
        for index in 1..=100u64 {
            assert_eq!(send(&mut gate, false, index, true), SendDecision::Transmit);
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
        assert_eq!(gate.may_present(2, false, false, 0, false), FrameTrust::Trusted);
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
    fn the_two_gates_agree_about_a_lost_frame() {
        // One timeline, both ends, with the reference field in play — the behaviour the two gates
        // have *together*, which neither can have alone.
        //
        // The server discards frame 2 on its way out. The client never acknowledges it, so the
        // encoder treats it as unconfirmed, and the frames behind it are chained to the last frame
        // the client did confirm — frame 1. The chain was never broken, so the client presents them,
        // and a single discarded frame costs a single frame instead of a hold.
        let mut send_gate = SendGate::new();
        let mut trust = TrustGate::new();

        // Frame 1: the opening keyframe, transmitted and trusted. The client decodes it.
        assert_eq!(send(&mut send_gate, true, 0, true), SendDecision::Transmit);
        assert_eq!(present(&mut trust, 1, true, false), FrameTrust::Trusted);

        // Frame 2 is discarded on the way out. It is never acknowledged.
        assert_eq!(send(&mut send_gate, false, 1, false), SendDecision::Transmit);
        assert_eq!(send_gate.discarded(), 1);

        // Frames 3 and 4 are encoded against frame 1 — the newest confirmed frame — and they go.
        assert_eq!(send(&mut send_gate, false, 1, true), SendDecision::Transmit);
        assert_eq!(send(&mut send_gate, false, 1, true), SendDecision::Transmit);
        assert!(
            !send_gate.is_suppressed(),
            "a discarded frame no longer stops the stream"
        );

        // The client sees 1, then 3: a hole of one frame, chained to a frame it decoded. It presents,
        // because nothing it will decode depends on the missing frame.
        assert_eq!(
            present_chained(&mut trust, 3, 1, true),
            FrameTrust::Trusted
        );
        assert_eq!(trust.trusted_across_gap(), 1);
        assert_eq!(trust.missed_frames(), 1, "the hole is still counted");
        assert!(trust.is_trusted());

        // And the case where the sender cannot say what the frame is chained to: the same timeline,
        // with the reference withheld. The client holds, exactly as it did before any of this.
        let mut trust = TrustGate::new();
        assert_eq!(present(&mut trust, 1, true, false), FrameTrust::Trusted);
        assert!(matches!(
            present(&mut trust, 3, false, false),
            FrameTrust::Untrusted {
                reason: UntrustedReason::Gap { missing: 1 },
                ..
            }
        ));
        assert!(!trust.is_trusted());
        assert_eq!(trust.missed_frames(), 1);
    }

    #[test]
    fn an_intact_frame_chained_to_an_unknown_reference_is_held() {
        // The other half of the reference rule, and the one that keeps it honest: the field is not a
        // licence to present anything. A contiguous, intact frame whose stated reference this client
        // never decoded is exactly the picture that decodes to something plausible and wrong.
        let mut gate = TrustGate::new();
        assert_eq!(present(&mut gate, 1, true, false), FrameTrust::Trusted);
        gate.on_decoder_result(true);

        // Frames 2 and 3 arrive contiguously, and both say they were encoded against frame 9, which
        // this client has never seen.
        assert_eq!(
            reason(present_chained(&mut gate, 2, 9, false)),
            UntrustedReason::UnconfirmedReference { reference: 9 }
        );
        assert!(!gate.is_trusted());
        assert_eq!(gate.untrusted_frames(), 1);
    }

    #[test]
    fn a_decoded_reference_is_remembered_as_a_window_of_indices() {
        // The client cannot ask "did I decode frame 7?" of a cursor: it decoded 5, 7 and 9, and a
        // cursor through 9 would answer yes for 6 and 8. The history is a bit per index.
        let mut history = DecodeHistory::default();
        for index in [5u64, 7, 9] {
            history.record(index);
        }
        assert!(history.contains(5));
        assert!(!history.contains(6));
        assert!(history.contains(7));
        assert!(!history.contains(8));
        assert!(history.contains(9));
        assert!(!history.contains(10), "a frame that has not been decoded");

        // Out of the window is out of knowledge: fail closed rather than remembering forever.
        let mut history = DecodeHistory::default();
        history.record(10);
        history.record(10 + DecodeHistory::WINDOW + 1);
        assert!(!history.contains(10));
        assert!(history.contains(10 + DecodeHistory::WINDOW + 1));
    }
}
