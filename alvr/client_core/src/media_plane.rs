//! The client's video receive path, on the media plane.
//!
//! `x-transport` has had a complete receiver since ADR-0013 — FEC, derived-nonce AEAD, a release
//! policy, `nack()` — and **nothing on the client used it**. The live path ran `alvr_sockets`, and
//! that is where the frames were being lost: a fixed 10-buffer pool that discarded datagrams
//! silently when it ran dry (doc 50 §A10). This module is the client half of the wiring.
//!
//! It puts the four pieces that must agree into one type, because they are only correct together
//! and every one of them has been wrong on its own at some point in this project:
//!
//! | piece | what it decides | what happens when it is missing |
//! |---|---|---|
//! | `x_transport::Receiver` | can this frame be rebuilt at all | a hole is displayed as a picture |
//! | `x_transport::TrustGate` | may this frame be shown | a broken reference chain is shown as grey |
//! | [`crate::stall::StuckDetector`] | how long to wait, and when to stop waiting | a hold never ends: a black screen |
//! | this module's counters | whether any of the above happened | two days lost to a silent discard |
//!
//! ## Where the datagrams come from
//!
//! [`DatagramSource`], so that the same receive path can be driven by a socket, by a recorded
//! stream, or by a lossy wrapper around either. The reference client does exactly this — its
//! transport is file-backed behind `-f <FILENAME>` and it can inject loss (`Fail this % of packets,
//! incoming on this side`) — and it is the reason its behaviour could be characterised without a
//! network. Ours could not, which is how a 1.5 % client-side discard came to look like a network
//! problem for two days.

use std::{
    thread,
    time::{Duration, Instant},
};

/// The rig sweep fixture (see `pump`): the datagrams-per-second a `FRAMESIM_READ_LIMIT` env var
/// asks this client to read at, or `None` when unset. Read once; a knob that flips mid-session
/// would make the sensor's history lie.
fn read_limit_per_sec() -> Option<f64> {
    static LIMIT: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("FRAMESIM_READ_LIMIT")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|limit| *limit > 0.0)
    })
}

use x_transport::{
    DeliveredFrame, FrameTrust, Receiver, RecvEvent, ReleasePolicy, UntrustedReason,
};

use crate::{
    latency::LatencyTrace,
    stall::{StuckAction, StuckDetector},
};

// The datagram vocabulary is `x-transport`'s, and the client used to have its own copy of it — two
// traits with the same shape, which is how the sending half came to have no binding at all. There is
// one trait now, one `SourceEvent`, and one implementation per medium.
pub use x_transport::{DatagramSource, SinkError, SourceEvent};

/// A recorded stream, replayed. Deterministic by construction: no clock, no network, no loss
/// except the loss that is in the recording.
#[derive(Debug, Clone, Default)]
pub struct ReplaySource {
    datagrams: Vec<Vec<u8>>,
    next: usize,
}

impl ReplaySource {
    pub fn new(datagrams: Vec<Vec<u8>>) -> Self {
        Self { datagrams, next: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.datagrams.len().saturating_sub(self.next)
    }
}

impl DatagramSource for ReplaySource {
    fn recv(&mut self, out: &mut Vec<u8>, _timeout: Duration) -> SourceEvent {
        let Some(datagram) = self.datagrams.get(self.next) else {
            return SourceEvent::Closed;
        };
        self.next += 1;
        out.clear();
        out.extend_from_slice(datagram);
        SourceEvent::Datagram
    }
}

/// Drops a deterministic fraction of datagrams.
///
/// The reference client has this built into the shipping binary; we did not, which meant the only
/// way to see how the client behaves on a bad link was to be on a bad link. A seeded generator
/// makes a run reproducible: the same seed and the same input give the same losses, so a
/// regression is a diff rather than a shrug.
#[derive(Debug)]
pub struct LossySource<S> {
    inner: S,
    /// xorshift64*. No dependency, and reproducible across machines — which a hash map's order is
    /// not, and neither is a system RNG.
    state: u64,
    /// Loss rate in parts per thousand, so 15 is 1.5 % — the figure this client was actually
    /// discarding before anyone could see it.
    permille: u32,
    consumed: u64,
    dropped: u64,
}

impl<S: DatagramSource> LossySource<S> {
    pub fn new(inner: S, seed: u64, permille: u32) -> Self {
        Self {
            inner,
            state: seed | 1,
            permille: permille.min(1000),
            consumed: 0,
            dropped: 0,
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    fn next_random(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl<S: DatagramSource> DatagramSource for LossySource<S> {
    fn recv(&mut self, out: &mut Vec<u8>, timeout: Duration) -> SourceEvent {
        loop {
            match self.inner.recv(out, timeout) {
                SourceEvent::Datagram => {
                    self.consumed += 1;
                    if (self.next_random() % 1000) < self.permille as u64 {
                        self.dropped += 1;
                        continue;
                    }
                    return SourceEvent::Datagram;
                }
                other => return other,
            }
        }
    }
}

// The UDP source used to live here, and it had a defect that nothing caught: it set the socket
// non-blocking once and then applied a read timeout per call, which on Linux is a no-op — so it
// answered "nothing there" for a socket with a datagram waiting on it, forever. It now lives in
// `alvr_sockets::media` next to the sending half, where both ends use one implementation and the
// test that would have caught it exists. See that module's docs.
pub use alvr_sockets::media::MediaSocket;
/// What the caller must do after a [`MediaPlaneReceiver::poll`].
///
/// Actions rather than symptoms: the plane does not know how to send a control packet or how to
/// talk to the decoder, and it should not. It decides, the client acts, and every decision is one of
/// four things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPlaneAction {
    /// Hand these bytes to the decoder. `frame_index` is the identity that joins this frame to what
    /// the server transmitted — the thing that let session 13 tell a server-side discard apart from a
    /// network loss, and the reason it is carried this far.
    Decode {
        frame_index: u64,
        /// The server's target display time for this frame, in the server's clock. The caller feeds
        /// it to `TimebaseOffset` to place it on its own clock and then to `FrameScheduler` to decide
        /// whether it is still worth showing.
        target_timestamp_us: u64,
        payload: Vec<u8>,
    },
    /// Nothing has progressed for [`crate::stall::ASK_FOR_KEYFRAME_AFTER`]. Ask the sender for a
    /// keyframe. The encoder inserts one only when asked, so this is the only way out of a hold.
    AskForKeyframe { stalled_for: Duration },
    /// Ask the sender to re-send these fragments: one round trip, one frame saved, no keyframe.
    Nack {
        frame_index: u64,
        fragments: Vec<u16>,
    },
    /// Nothing has progressed for [`crate::stall::HARD_RESET_AFTER`]. Asking has failed.
    Reset { stalled_for: Duration },
    /// The client's own queueing delay, to be carried back to the sender. The only message either
    /// end sends that is about the *receiver* rather than a frame.
    QueueDelay {
        micros: u32,
        late_per_mille: u16,
        missing_per_mille: u16,
        read_per_sec: u32,
    },
}

/// The client's video receive path, driven by a socket.
///
/// [`VideoPlane`] decides and this feeds it. Deliberately **not** threaded: the client already owns
/// a receive thread and a timing discipline, and moving the socket read into a second thread is
/// precisely the change that made the old reader discard 1.5 % of the datagrams it had already read
/// without recording it (`doc 50 §A10`). One thread, one place where a datagram can be lost, and a
/// counter for it.
pub struct MediaPlaneReceiver {
    source: MediaSocket,
    plane: VideoPlane,
}

impl MediaPlaneReceiver {
    /// `socket` is the caller's: the port and the QoS marking come from the session, and a media
    /// plane that bound its own would be a second source of truth for both.
    ///
    /// `keys` is the session's media schedule, derived from the control channel's Noise exchange
    /// and never transmitted. `None` runs the plane in the clear, which is what a bench or a
    /// recording wants and what a session must not do.
    pub fn new(
        socket: MediaSocket,
        policy: ReleasePolicy,
        keys: Option<x_transport::KeySchedule>,
        now: Instant,
    ) -> Self {
        Self {
            source: socket,
            plane: VideoPlane::new(policy, keys, now),
        }
    }

    /// Read what has arrived, release what is ready, and report what to do.
    ///
    /// The trace is passed in rather than owned so the *other* boundaries — decode, submit, display
    /// — can be marked by whoever owns them. One trace, one clock, and the join between the two ends
    /// of the pipeline is the frame index that is already on the wire.
    ///
    /// Returns `false` once the socket can never deliver again, so the caller stops rather than
    /// spinning. A quiet socket is not that: it returns an empty action list and `true`.
    pub fn poll(
        &mut self,
        trace: &mut LatencyTrace,
        now: Instant,
        budget: Duration,
    ) -> (Vec<MediaPlaneAction>, bool) {
        let open = self.plane.pump(&mut self.source, now, budget);

        let actions = self
            .plane
            .release(now)
            .into_iter()
            .filter_map(|event| match event {
                PlaneEvent::Present {
                    frame_index,
                    target_timestamp_us,
                    payload,
                } => {
                    // The frame is reconstructed and trustworthy: this is the start of every span
                    // that matters, and the only mark that creates a record.
                    trace.frame_received(frame_index, target_timestamp_us, now);
                    Some(MediaPlaneAction::Decode {
                        frame_index,
                        target_timestamp_us,
                        payload,
                    })
                }
                // A held frame is a decision, not an action: the previous image stays and the
                // compositor reprojects it. `PlaneStats` records that it happened, which is what
                // the old reader did not do.
                PlaneEvent::Held { .. } => None,
                PlaneEvent::Nack {
                    frame_index,
                    fragments,
                } => Some(MediaPlaneAction::Nack {
                    frame_index,
                    fragments,
                }),
                PlaneEvent::AskForKeyframe { stalled_for } => {
                    Some(MediaPlaneAction::AskForKeyframe { stalled_for })
                }
                PlaneEvent::Reset { stalled_for } => Some(MediaPlaneAction::Reset { stalled_for }),
                PlaneEvent::QueueDelay {
                    micros,
                    late_per_mille,
                    missing_per_mille,
                    read_per_sec,
                } => Some(MediaPlaneAction::QueueDelay {
                    micros,
                    late_per_mille,
                    missing_per_mille,
                    read_per_sec,
                }),
            })
            .collect();

        (actions, open)
    }

    pub fn stats(&self) -> &PlaneStats {
        self.plane.stats()
    }

    /// Tell the plane a frame was decoded. See [`VideoPlane::on_decoded`].
    ///
    /// On the receiver rather than on the socket, because the plane is what the answer means
    /// something to: the trust gate consults it for the *next* frame's reference.
    pub fn on_decoded(&mut self, frame_index: u64) -> bool {
        self.plane.on_decoded(frame_index)
    }

    /// Tell the plane the decoder refused a frame. See [`VideoPlane::on_decode_failed`].
    pub fn on_decode_failed(&mut self, frame_index: u64) {
        self.plane.on_decode_failed(frame_index);
    }

    /// Count an acknowledgement the caller sent. See [`VideoPlane::count_ack_sent`].
    pub fn count_ack_sent(&mut self) {
        self.plane.count_ack_sent()
    }

    /// The newest decoded frame and its bitmap. See [`VideoPlane::decoded_snapshot`].
    pub fn decoded_snapshot(&self) -> Option<(u64, u64)> {
        self.plane.decoded_snapshot()
    }

    /// The receiver's own account. See [`VideoPlane::receiver_account`].
    pub fn receiver_account(&self) -> String {
        self.plane.receiver_account()
    }

    /// What the failed frames actually had. See [`VideoPlane::failed_frame_account`].
    pub fn failed_frame_account(&self) -> String {
        self.plane.failed_frame_account()
    }

    /// The address the last datagram came from.
    ///
    /// The server sends the media stream from an ephemeral port, so this is the only place the
    /// client can learn where a repair has to go back to.
    pub fn last_sender(&self) -> Option<std::net::SocketAddr> {
        self.source.last_sender()
    }

    /// Datagrams that arrived from a host other than the streamer. Non-zero means something else on
    /// the network is talking to this port — worth knowing before wondering why frames are corrupt.
    pub fn datagrams_from_elsewhere(&self) -> u64 {
        self.source.datagrams_from_elsewhere()
    }

    pub fn oversized_datagrams(&self) -> u64 {
        self.source.datagrams_oversized()
    }

    /// Datagrams read off the socket, whatever became of them. The one number that separates "the
    /// wire lost it" from "we never looked".
    pub fn datagrams_received(&self) -> u64 {
        self.source.datagrams_received()
    }
}

/// What the receive path decided. One of these per frame, plus the ladder's escalations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaneEvent {
    /// Rebuilt and trustworthy: show it, and here are its bytes.
    ///
    /// The payload is carried, not looked up: the receiver has already assembled it, and a frame
    /// whose bytes are fetched from somewhere else at display time is a frame that can be fetched
    /// after it has been recycled.
    Present {
        frame_index: u64,
        /// The time the server aimed this frame at, in the **server's** clock. This is the input to
        /// `x_transport::TimebaseOffset` and then to `FrameScheduler`: without it the client can
        /// decide *what* to show but never *whether showing it now is showing the past*.
        target_timestamp_us: u64,
        payload: Vec<u8>,
    },
    /// Not trustworthy: **hold the last good frame**. ADR-0011 — this is not a failure to show a
    /// frame, it is the permitted response to one that cannot be trusted.
    Held {
        frame_index: u64,
        reason: UntrustedReason,
    },
    /// Ask the sender to re-send these fragments of this frame. The repair that costs one round
    /// trip instead of one frame.
    Nack {
        frame_index: u64,
        fragments: Vec<u16>,
    },
    /// No progress for the ask threshold: ask the sender for a keyframe.
    AskForKeyframe { stalled_for: Duration },
    /// No progress for the reset threshold. Asking has failed.
    Reset { stalled_for: Duration },
    /// How far behind the client is reading, for the sender. See
    /// [`FeedbackOutcome::QueueDelay`](x_transport::FeedbackOutcome::QueueDelay).
    QueueDelay {
        micros: u32,
        late_per_mille: u16,
        missing_per_mille: u16,
        read_per_sec: u32,
    },
}

/// Counters, because a receive path whose behaviour is only visible in the picture cannot be
/// debugged — which is how the silent discard survived two days of investigation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaneStats {
    pub datagrams_received: u64,
    /// Datagrams a source refused to deliver (loss injection, or a recording's own gaps).
    pub datagrams_dropped_by_source: u64,
    /// Datagrams the receiver rejected: malformed, unauthenticated, or late.
    pub datagrams_rejected: u64,
    pub frames_presented: u64,
    /// Frames the display rule refused. Every released frame is either presented or held, so
    /// `frames_presented + frames_held` is the number of frames that reached the display path.
    pub frames_held: u64,
    /// Frames the transport could not rebuild. **A subset of `frames_held`**, not a third bucket:
    /// a frame with no payload is held by definition. Counted separately because they mean
    /// different things — `held` is the gate working, `abandoned` is the link being worse than the
    /// code rate.
    pub frames_abandoned: u64,
    pub frames_repaired: u64,
    pub nacks_sent: u64,
    /// Frames whose missing shards were **not** asked for, because the client was missing too much of
    /// every frame for a repair to be the answer. See [`NACK_SUPPRESS_MISSING_PERMILLE`].
    pub nacks_suppressed: u64,
    pub keyframe_requests: u64,
    pub resets: u64,
    /// Queue-delay reports sent to the sender. See [`PlaneEvent::QueueDelay`].
    pub queue_delay_reports: u64,
    /// Keyframes *released to the display path*, and how many of them had a payload.
    ///
    /// A keyframe is the only thing that clears the trust gate, so "did a clean keyframe ever
    /// arrive" is the difference between a hold that ends and a hold that is permanent — and
    /// without these two numbers those look identical from every other counter.
    pub keyframes_in: u64,
    pub keyframes_clean: u64,
    /// Why frames were held, by reason. The gate has four, and they call for different fixes:
    /// a missing keyframe is a request that did not land, a gap is loss, `DatagramLoss` is a hole
    /// inside the frame, and `DecoderRejected` is the decoder.
    pub held_no_keyframe: u64,
    pub held_gap: u64,
    pub held_datagram_loss: u64,
    pub held_decoder: u64,
    /// Frames held because they were chained to a frame this client never decoded. Distinct from
    /// `held_gap`, and the distinction is the point: a gap can be *routed around* when the sender
    /// says what the frame is chained to, and this counter is the case where it cannot be.
    pub held_unconfirmed_reference: u64,
    /// Frames presented **across a hole** because their reference was one this client had decoded,
    /// and frames acknowledged decoded. The second is what the sender's encoder needs; the first is
    /// what it buys, and both are zero on a link with no loss.
    pub frames_trusted_across_gap: u64,
    pub acks_sent: u64,
    /// The newest read rate the client measured, for the summary. See
    /// [`x_transport::Receiver::read_per_sec`].
    pub read_per_sec: u32,
}

impl PlaneStats {
    pub fn summary(&self) -> String {
        format!(
            "video plane: {} datagrams in ({} dropped by source, {} rejected), {} frames presented \
             ({} across a hole on a confirmed reference), {} held ({} no-keyframe, {} gap, {} \
             datagram-loss, {} unconfirmed-reference, {} decoder), {} abandoned, \
             {} repaired by FEC, {} keyframe(s) in ({} clean), {} nack(s) ({} suppressed while \
             drowning), {} keyframe request(s), {} reset(s), {} ack(s); read {} datagram(s)/s",
            self.datagrams_received,
            self.datagrams_dropped_by_source,
            self.datagrams_rejected,
            self.frames_presented,
            self.frames_trusted_across_gap,
            self.frames_held,
            self.held_no_keyframe,
            self.held_gap,
            self.held_datagram_loss,
            self.held_unconfirmed_reference,
            self.held_decoder,
            self.frames_abandoned,
            self.frames_repaired,
            self.keyframes_in,
            self.keyframes_clean,
            self.nacks_sent,
            self.nacks_suppressed,
            self.keyframe_requests,
            self.resets,
            self.acks_sent,
            self.read_per_sec,
        )
    }
}

/// How often the client reports on itself. A quarter of a second: short enough that the sender's
/// budget follows a changing link, long enough that the numbers are about the link rather than about
/// one frame — and, unlike a frame count, it does not become infinite when frames stop completing.
const REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// How long the read-rate sensor takes to close a window.
///
/// A quarter of the second the sender hears about, and the same period on purpose: the number in a
/// report is then the fastest the client read in the interval the report covers, rather than a
/// maximum carried over from an older one.
const READ_SENSOR_WINDOW: Duration = Duration::from_millis(250);

/// Above this share of a frame's declared shards missing, the client stops asking for repairs.
///
/// **A client that is drowning does not ask for more water.** A repair is a datagram like any other
/// and it crosses the same link, so a client that is missing a large share of *every* frame is a
/// client whose requests become the congestion: measured on the live rig, 20 229 retransmitted
/// datagrams against 43 229 sent — every second datagram on the wire was a repair the client had
/// asked for, and 10 686 of them arrived after the frame they were for had been given up on. The
/// ask cannot be right when the receiver is the bottleneck; the answer to that is for the *sender*
/// to send less, which is what the same report carries (see `Feedback::QueueDelay`).
///
/// Ten percent is the point where a frame is unlikely to survive on its own stragglers anyway, and
/// well above the sub-one-percent a healthy link shows.
const NACK_SUPPRESS_MISSING_PERMILLE: u32 = 100;

/// The client's video receive path.
pub struct VideoPlane {
    receiver: Receiver,
    trust: x_transport::TrustGate,
    stall: StuckDetector,
    started: Instant,
    stats: PlaneStats,
    /// Frames released since the last report, kept for the summary's shape rather than for cadence:
    /// the report itself is on a clock, so that a client completing nothing still reports.
    frames_since_queue_report: u32,
    /// When the last report went out.
    last_report: Instant,
    /// Whether this plane has ever reported. The first report is sent **immediately**, not after a
    /// quarter of a second: it is the one that tells the sender what the client can read, and every
    /// mechanism downstream — the budget, the frame size, the stop-and-wait retry — is waiting for it.
    never_reported: bool,
    /// Which frame indices this client has **decoded**, bounded to the last
    /// [`x_transport::DecodeHistory::WINDOW`] indices.
    ///
    /// This is the client's half of the reference contract: the sender tells each frame what it was
    /// encoded against, and this is what lets the client answer whether it has that picture. It is
    /// filled from the decoder's answer, not from the transport's — a frame whose bytes arrived and
    /// whose decode failed is not one a later frame may be built on.
    decoded: x_transport::DecodeHistory,
    /// Frames this client **showed** — released with a payload and trusted by the gate.
    ///
    /// Distinct from `decoded`, and the distinction is a batch: a release pass can hand over twenty
    /// frames at once after a freeze, and every one of them references the frame before it, which is
    /// in that same batch. Asking the decoder's history would hold nineteen of the twenty for a frame
    /// the client is going to decode a millisecond later. A frame that was *shown* is what the frames
    /// behind it need, and a decode that then fails is recorded against it below.
    shown: x_transport::DecodeHistory,
    /// Frames the decoder did not take. A frame here is not a reference, whatever it was shown as.
    failed: x_transport::DecodeHistory,
    /// The receiver's late-datagram count at the last queue report, so each report carries the
    /// delta rather than the running total, and the repair requests the same interval made, so the
    /// delta can be expressed as a share of them 2014 which is the only form the sender can apply
    /// without having to agree on an interval.
    last_late_reported: u64,
    last_nacks_reported: u64,
}

impl VideoPlane {
    pub fn new(
        policy: ReleasePolicy,
        keys: Option<x_transport::KeySchedule>,
        now: Instant,
    ) -> Self {
        Self {
            receiver: Receiver::new(policy, keys.map(x_transport::MediaKeys::rotating)),
            trust: x_transport::TrustGate::new(),
            stall: StuckDetector::new(now),
            started: now,
            stats: PlaneStats::default(),
            frames_since_queue_report: 0,
            last_report: now,
            never_reported: true,
            decoded: x_transport::DecodeHistory::default(),
            shown: x_transport::DecodeHistory::default(),
            failed: x_transport::DecodeHistory::default(),
            last_late_reported: 0,
            last_nacks_reported: 0,
        }
    }

    pub fn stats(&self) -> &PlaneStats {
        &self.stats
    }

    /// The receiver's own account of what it rebuilt — and, for what it could not, how many shards
    /// were missing against how many parity shards it had. Those two numbers together are the
    /// difference between "the repair path is broken" and "the link was worse than the code rate",
    /// and they had never been printed.
    pub fn receiver_account(&self) -> String {
        let s = self.receiver.stats();
        let failed = s.frames_unreconstructable.max(1);
        format!(
            "receiver: {} complete, {} recovered, {} unreconstructable ({:.1} erasures vs {:.1} \
             parity on average, worst {}), {} late, {} duplicate, {} retransmit, {} fragment(s) rebuilt",
            s.frames_complete,
            s.frames_recovered,
            s.frames_unreconstructable,
            s.unreconstructable_erasures as f64 / failed as f64,
            s.unreconstructable_parity as f64 / failed as f64,
            s.max_erasures,
            s.datagrams_late,
            s.datagrams_duplicate,
            s.datagrams_retransmit,
            s.fragments_repaired,
        )
    }

    /// What the frames that could not be rebuilt actually had, and how long they were given.
    pub fn failed_frame_account(&self) -> String {
        let s = self.receiver.stats();
        let failed = s.frames_unreconstructable.max(1);
        format!(
            "failed frames: waited {:.1} ms on average, had {:.1} of {:.1} shards when declared",
            s.failed_waited_us as f64 / failed as f64 / 1000.0,
            s.failed_shards_present as f64 / failed as f64,
            s.failed_shards_total as f64 / failed as f64,
        )
    }

    /// Milliseconds since this plane was created — the receiver's time base.
    fn clock(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// Pull datagrams until the source has nothing more, and hand each to the receiver.
    ///
    /// Returns whether the source is still open. A `Closed` source is not an error: a replay ends.
    ///
    /// `budget` bounds how long to keep pulling; **zero means drain**, which is what a replay or a
    /// test wants. A live socket needs no budget either — `recv` reports `Timeout` when it is
    /// empty and the loop stops there — so the budget exists only for a source that never yields.
    pub fn pump(
        &mut self,
        source: &mut impl DatagramSource,
        now: Instant,
        budget: Duration,
    ) -> bool {
        let clock = self.clock(now);
        let mut buffer = Vec::with_capacity(1500);
        let deadline = if budget.is_zero() {
            None
        } else {
            Some(Instant::now() + budget)
        };

        // The rig sweep fixture (ROADMAP coverage gap): `FRAMESIM_READ_LIMIT` paces the reads to
        // simulate a client that cannot keep up, which is the client the budget and the ladder
        // were built for and the one qemu at full speed refuses to be. Inert unless the env var
        // is set to a positive datagrams-per-second number. The pacing sits *before* the read so
        // the socket buffer overflows like a real slow client's, not like a paused one.
        let read_limit = read_limit_per_sec();
        let mut last_read = Instant::now();

        loop {
            if let Some(limit) = read_limit {
                let min_interval = Duration::from_secs_f64(1.0 / limit);
                let elapsed = last_read.elapsed();
                if elapsed < min_interval {
                    thread::sleep(min_interval - elapsed);
                }
            }
            last_read = Instant::now();

            match source.recv(&mut buffer, Duration::ZERO) {
                SourceEvent::Datagram => {
                    self.stats.datagrams_received += 1;
                    // Every datagram feeds the read-rate sensor *and* the receiver: the sensor is of
                    // what was read, which is a fact about datagrams that were late, duplicated or
                    // foreign as much as about the ones that went into a frame.
                    self.receiver.observe_read_rate(clock, READ_SENSOR_WINDOW);
                    match self.receiver.on_datagram(&buffer, clock) {
                        RecvEvent::Rejected(_) => self.stats.datagrams_rejected += 1,
                        // Duplicates and late arrivals are counted by the receiver's own stats;
                        // neither is a reason to stop.
                        RecvEvent::Accepted { .. } | RecvEvent::Duplicate | RecvEvent::Late => {}
                    }
                }
                SourceEvent::Timeout => break,
                SourceEvent::Closed => return false,
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
        }
        true
    }

    /// Release whatever is ready and decide, for each frame, whether it may be shown.
    pub fn release(&mut self, now: Instant) -> Vec<PlaneEvent> {
        let clock = self.clock(now);
        let mut events = Vec::new();
        let mut presented_this_round = false;

        for frame in self.receiver.release(clock) {
            let frame_index = frame.frame_index;
            match self.classify(&frame, now) {
                Decision::Present => {
                    presented_this_round = true;
                    events.push(PlaneEvent::Present {
                        frame_index,
                        target_timestamp_us: frame.target_timestamp_us,
                        payload: frame.into_payload().unwrap_or_default(),
                    });
                }
                Decision::Held(reason) => events.push(PlaneEvent::Held {
                    frame_index,
                    reason,
                }),
            }
        }

        if presented_this_round {
            self.stall.progress(now);
        } else {
            match self.stall.poll(now) {
                StuckAction::AskForKeyframe => {
                    self.stats.keyframe_requests += 1;
                    events.push(PlaneEvent::AskForKeyframe {
                        stalled_for: self.stall.stalled_for(now),
                    });
                }
                StuckAction::Reset => {
                    self.stats.resets += 1;
                    events.push(PlaneEvent::Reset {
                        stalled_for: self.stall.stalled_for(now),
                    });
                }
                StuckAction::Progress => {}
            }
        }

        // Ask for the fragments of anything still incomplete. The receiver knows which ones are
        // missing; this is the round trip that saves a frame instead of a keyframe.
        //
        // This is deliberately the only place the ask is made, and deliberately ungated. Two attempts
        // at making it cleverer were measured on the rig and both were worse:
        //
        // - Moving it into `pump` to go out sooner (`c73255bf`) put the ask *before* the frame's own
        //   shards had finished arriving. On this rig the client's socket carries a backlog, so the
        //   "missing" set at that moment is mostly datagrams already on their way in — the requests
        //   named them, the sender answered them, and the repair traffic became the congestion.
        //   Measured: 6.6 k requests answered with 35 k datagrams, and 40 frames presented against
        //   927 for this version.
        // - Rate-limiting it per round trip without moving it (`03e2e961`) left one ask where there
        //   had been several, and that one ask landed after the release pass — past the window.
        //
        // The floor under the whole idea is that a client which is behind cannot distinguish "this
        // shard is lost" from "this shard is still queued behind me". The fix for that is to stop
        // being behind, not to tune the question — which is what the queue-delay report below is for.
        //
        // And there is one condition under which it is not made at all: see
        // [`NACK_SUPPRESS_MISSING_PERMILLE`]. The timing experiments above were about *when* to ask;
        // this is about *whether*, and the difference is that the answer is now measurable — the
        // client knows what share of every frame it is failing to receive, and while that is most of
        // a frame, asking for more of it is asking the sender to add to the congestion.
        let drowning = self.receiver.stats().missing_permille > NACK_SUPPRESS_MISSING_PERMILLE;
        for frame_index in self.receiver.incomplete_frames() {
            let fragments = self.receiver.nack(frame_index);
            if fragments.is_empty() {
                continue;
            }
            if drowning {
                self.stats.nacks_suppressed += 1;
                continue;
            }
            self.stats.nacks_sent += fragments.len() as u64;
            events.push(PlaneEvent::Nack {
                frame_index,
                fragments,
            });
        }

        // Tell the sender how far behind the client is reading, and how fast it can read at all.
        //
        // **Time-based, not frame-based.** The cadence was thirty frames, which is a different real
        // interval on every link and *infinite* on a client that completes nothing — and a client that
        // completes nothing is exactly the one the sender needs to hear from. A quarter of a second is
        // a quarter of a second whether or not a frame made it.
        self.frames_since_queue_report += 1;
        let due = now.saturating_duration_since(self.last_report) >= REPORT_INTERVAL;
        if due || self.never_reported {
            self.never_reported = false;
            self.last_report = now;
            let micros = self.receiver.queue_delay_us();
            // The late count since the last report: fragments that arrived *after* their frame was
            // released. They were asked for, they came, and they were discarded — so they are the
            // part of the sender's loss signal that is not loss. See `Feedback::QueueDelay`.
            let late_total = self.receiver.stats().datagrams_late;
            let late = late_total.saturating_sub(self.last_late_reported);
            self.last_late_reported = late_total;

            let nacks_total = self.stats.nacks_sent;
            let nacks = nacks_total.saturating_sub(self.last_nacks_reported);
            self.last_nacks_reported = nacks_total;
            let late_per_mille = if nacks == 0 {
                0
            } else {
                ((late * 1000) / nacks).min(1000) as u16
            };

            // The share of each frame's declared shards that never arrived. This is the number the
            // drain delay is blind to: measured only on frames that completed, it flatters a client
            // that is losing the large ones — 21 ms of "queueing" on a client reading a third of the
            // stream, which on the live rig left the ladder on its mildest rung while every frame
            // became a hole.
            let missing_per_mille = self.receiver.stats().missing_permille.min(1000) as u16;
            // The sensor that works when nothing completes. Taken as the maximum over the interval
            // just ended, so a burst of traffic is not averaged away by the quiet around it.
            let read_per_sec = self.receiver.take_read_rate();
            self.stats.read_per_sec = read_per_sec;

            self.stats.queue_delay_reports += 1;
            events.push(PlaneEvent::QueueDelay {
                micros: micros.min(u32::MAX as u64) as u32,
                late_per_mille,
                missing_per_mille,
                read_per_sec,
            });
        }

        events
    }

    /// Apply the display rule to one released frame, counting what it decides.
    fn classify(&mut self, frame: &DeliveredFrame, now: Instant) -> Decision {
        let usable = frame.is_displayable();
        if frame.is_keyframe {
            self.stats.keyframes_in += 1;
            if usable {
                self.stats.keyframes_clean += 1;
            }
        }
        if !usable {
            self.stats.frames_abandoned += 1;
        } else if matches!(frame.outcome, x_transport::FrameOutcome::Recovered { .. }) {
            self.stats.frames_repaired += 1;
        }

        // The one question the reference field exists to answer: is the picture this frame was
        // encoded against one this client decoded? A frame whose chain is confirmed is decodable
        // *across* a hole, so a missing frame stops being a hold — which is what turns one lost frame
        // from ~140 held frames into one stutter.
        let reference_decoded = frame.reference_frame != 0
            && frame.reference_frame < frame.frame_index
            && self.shown.contains(frame.reference_frame)
            && !self.failed.contains(frame.reference_frame);

        // A frame with a payload has no hole in it, repaired or not — the transport only hands out
        // bytes it could rebuild. A frame without one is the only case that may not be shown.
        match self.trust.may_present(
            frame.frame_index,
            frame.is_keyframe,
            !usable,
            frame.reference_frame,
            reference_decoded,
        ) {
            FrameTrust::Trusted => {
                self.stats.frames_presented += 1;
                self.stats.frames_trusted_across_gap = self.trust.trusted_across_gap();
                // Shown, so the frames behind it may be built on it. Recorded now rather than when
                // the decoder answers, because a release pass can hand over a whole batch that
                // references itself frame by frame — see the note on `shown`.
                self.shown.record(frame.frame_index);
                self.stall.progress(now);
                Decision::Present
            }
            FrameTrust::Untrusted { reason, .. } => {
                self.stats.frames_held += 1;
                match reason {
                    x_transport::UntrustedReason::NoKeyframeYet => self.stats.held_no_keyframe += 1,
                    x_transport::UntrustedReason::Gap { .. } => self.stats.held_gap += 1,
                    x_transport::UntrustedReason::DatagramLoss => {
                        self.stats.held_datagram_loss += 1
                    }
                    x_transport::UntrustedReason::UnconfirmedReference { .. } => {
                        self.stats.held_unconfirmed_reference += 1
                    }
                    x_transport::UntrustedReason::DecoderRejected => self.stats.held_decoder += 1,
                }
                Decision::Held(reason)
            }
        }
    }

    /// The newest frame this plane has decoded and the bitmap behind it, for an acknowledgement.
    /// See [`x_transport::DecodeHistory::snapshot`].
    pub fn decoded_snapshot(&self) -> Option<(u64, u64)> {
        self.decoded.snapshot()
    }

    /// Tell the plane that a frame was decoded and is in the decoder's reference chain.
    ///
    /// Called by the caller after the decoder has *accepted* the frame, because that — not the
    /// datagrams arriving — is what makes a frame a usable reference. Returns whether this is news
    /// worth sending: the caller acknowledges exactly what this returns true for, so a duplicate or a
    /// reorder does not turn into a packet.
    pub fn on_decoded(&mut self, frame_index: u64) -> bool {
        self.decoded.record(frame_index)
    }

    /// Tell the plane the decoder did **not** take a frame it was shown.
    ///
    /// The frame is then not a reference for anything behind it, whatever it was shown as. Recorded
    /// rather than forgotten, because the two histories answer different questions: what was shown,
    /// and what was actually usable.
    pub fn on_decode_failed(&mut self, frame_index: u64) {
        self.failed.record(frame_index);
        self.trust.on_decoder_result(false);
    }

    /// Count an acknowledgement the caller sealed and sent. Kept here rather than in the caller so
    /// the plane's summary is the one place the whole exchange is visible.
    pub fn count_ack_sent(&mut self) {
        self.stats.acks_sent += 1;
    }
}

/// What [`VideoPlane::classify`] decided, before the payload is attached. Separate from
/// [`PlaneEvent`] because the decision is about the *frame* and the event carries its bytes, and the
/// bytes can only be moved out once the borrow that decided is over.
enum Decision {
    Present,
    Held(UntrustedReason),
}

#[cfg(test)]
mod tests {
    use super::*;
    use x_transport::{Packetizer, ParityPolicy};

    const MTU: usize = 1400;
    const SHARD: usize = MTU - x_transport::HEADER_LEN;

    fn payload(shards: usize) -> Vec<u8> {
        (0..shards * SHARD).map(|i| (i % 251) as u8).collect()
    }

    /// Packetise `count` frames at `parity` overhead, marking every `keyframe_every`.
    fn stream(
        count: u64,
        shards: usize,
        parity: ParityPolicy,
        keyframe_every: u64,
    ) -> Vec<Vec<u8>> {
        let packetizer = Packetizer::new(MTU, parity);
        let bytes = payload(shards);
        let mut seq = 0;
        let mut out = Vec::new();
        for frame_index in 1..=count {
            let (_, datagrams) = packetizer
                .fragment(
                    x_transport::FrameMeta {
                        frame_index,
                        target_timestamp_us: frame_index * 11_111,
                        is_keyframe: frame_index == 1 || frame_index.is_multiple_of(keyframe_every),
                        key_epoch: 0,
                        reference_frame: 0,
                    },
                    &bytes,
                    &mut seq,
                    None,
                )
                .unwrap();
            out.extend(datagrams);
        }
        out
    }

    /// Release policy: no jitter window, a tight deadline — a test should not wait.
    fn policy() -> ReleasePolicy {
        ReleasePolicy::new(0, Duration::from_millis(5))
    }

    #[test]
    fn a_clean_stream_presents_every_frame() {
        let mut plane = VideoPlane::new(policy(), None, Instant::now());
        let mut source = ReplaySource::new(stream(10, 4, ParityPolicy::Off, 30));

        let now = Instant::now();
        // `false` here means the replay ran out — which is how a recording ends, not a failure.
        let still_open = plane.pump(&mut source, now, Duration::ZERO);
        assert!(!still_open, "a finished replay should report itself closed");
        let events = plane.release(now + Duration::from_millis(50));

        let presented = events
            .iter()
            .filter(|e| matches!(e, PlaneEvent::Present { .. }))
            .count();
        assert_eq!(presented, 10, "a clean stream lost frames: {events:?}");
        assert_eq!(plane.stats().frames_held, 0);
        assert_eq!(plane.stats().frames_abandoned, 0);
        assert_eq!(plane.stats().frames_presented, 10);
    }

    #[test]
    fn loss_inside_the_fec_budget_is_repaired_and_never_held() {
        // The whole point of the media plane: a datagram is lost, the frame is not.
        let mut plane = VideoPlane::new(policy(), None, Instant::now());
        let mut source = LossySource::new(
            ReplaySource::new(stream(20, 8, ParityPolicy::Ratio { fraction: 0.25 }, 30)),
            0x5EED,
            20, // 2 % of datagrams
        );

        let mut presented = 0;
        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);
        for event in plane.release(now + Duration::from_millis(50)) {
            match event {
                PlaneEvent::Present { .. } => presented += 1,
                PlaneEvent::Held { reason, .. } => {
                    panic!("held a frame at 2 % loss with 25 % parity: {reason:?}")
                }
                _ => {}
            }
        }

        assert!(source.dropped() > 0, "the test injected no loss");
        assert_eq!(
            presented,
            20,
            "{} frames lost to {} dropped datagrams despite 25 % parity",
            plane.stats().frames_abandoned,
            source.dropped()
        );
    }

    #[test]
    fn loss_beyond_the_budget_is_held_and_never_presented() {
        // ADR-0011. With no parity, a dropped datagram loses a frame; a frame that could not be
        // rebuilt must not reach the display path at all.
        let mut plane = VideoPlane::new(policy(), None, Instant::now());
        let mut source = LossySource::new(
            ReplaySource::new(stream(40, 8, ParityPolicy::Off, 30)),
            0xC0FFEE,
            60, // 6 % of datagrams: whole frames will be lost
        );

        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);
        for event in plane.release(now + Duration::from_millis(50)) {
            assert!(
                !matches!(event, PlaneEvent::Present { payload, .. } if payload.is_empty()),
                "presented a frame with no payload"
            );
        }
        assert!(
            plane.stats().frames_abandoned > 0,
            "the test did not actually lose a frame"
        );
        assert_eq!(
            plane.stats().frames_presented + plane.stats().frames_held,
            40,
            "a released frame was neither presented nor held"
        );
        assert!(
            plane.stats().frames_abandoned <= plane.stats().frames_held,
            "a frame with no payload was somehow not held"
        );
        // A held frame is the invariant working; a presented unreconstructable one would be the
        // bug. `frames_abandoned` counts the latter, and it must equal the frames that had no
        // payload — which is exactly what `Held` records. So: nothing abandoned-then-presented.
        assert!(
            plane.stats().frames_held > 0,
            "no hold was recorded for the frames that could not be rebuilt"
        );
    }

    #[test]
    fn a_hole_does_not_hold_a_frame_chained_to_one_the_client_decoded() {
        // **The payoff of the reference field, end to end through the plane.**
        //
        // Frame 5 is lost entirely. On the old rule every frame after it was held until the next
        // keyframe — one lost frame, a frozen picture. Here the sender says what each frame was
        // encoded against, and after the hole the encoder references frame 4, the newest frame the
        // client acknowledged. The client decoded frame 4, so the frames behind the hole are
        // presentable: nothing they decode depends on frame 5.
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(4);
        let mut seq = 0;
        let mut datagrams = Vec::new();
        for frame_index in 1..=40u64 {
            // What the encoder does with the acknowledgements it has: reference the previous frame,
            // and across a hole reference the newest frame the client confirmed.
            let reference = if frame_index <= 4 { frame_index - 1 } else { 4 };
            let (_, d) = packetizer
                .fragment(
                    x_transport::FrameMeta {
                        frame_index,
                        target_timestamp_us: frame_index * 11_111,
                        is_keyframe: frame_index == 1,
                        key_epoch: 0,
                        reference_frame: reference,
                    },
                    &bytes,
                    &mut seq,
                    None,
                )
                .unwrap();
            // Frame 5 never arrives — not one datagram of it.
            if frame_index != 5 {
                datagrams.extend(d);
            }
        }

        let mut plane = VideoPlane::new(policy(), None, Instant::now());
        let now = Instant::now();

        // First pass: frames 1..4 are complete, so they are released and shown, and the caller
        // tells the plane they decoded. This is the sequence a live client produces.
        {
            // Only what belongs to frames 1..4, so the pass is exactly what the client sees before
            // the hole: four clean pictures and no knowledge of what comes next.
            let mut partial = Vec::new();
            for datagram in datagrams.iter() {
                let (header, _) = x_transport::FragmentHeader::decode(datagram).unwrap();
                if header.frame_index <= 4 {
                    partial.push(datagram.clone());
                }
            }
            let mut source = ReplaySource::new(partial);
            plane.pump(&mut source, now, Duration::ZERO);
        }
        for event in plane.release(now + Duration::from_millis(20)) {
            if let PlaneEvent::Present { frame_index, .. } = event {
                assert!(
                    plane.on_decoded(frame_index),
                    "frame {frame_index} decoded twice"
                );
            }
        }
        assert_eq!(plane.stats().frames_presented, 4);
        assert_eq!(plane.stats().frames_held, 0);

        // Second pass: everything else. Frame 5 is abandoned (the stream has moved on without it),
        // and the frames behind it are presented because their reference — frame 4 — is one the
        // client decoded.
        let mut source = ReplaySource::new(datagrams);
        plane.pump(&mut source, now, Duration::ZERO);
        let events = plane.release(now + Duration::from_millis(80));

        let presented: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                PlaneEvent::Present { frame_index, .. } => Some(*frame_index),
                _ => None,
            })
            .collect();
        assert!(
            presented.contains(&6) && presented.contains(&40),
            "the frames behind the hole were held: {presented:?}"
        );
        assert!(
            !presented.contains(&5),
            "the frame that never arrived was presented"
        );
        assert!(
            plane.stats().frames_trusted_across_gap > 0,
            "no frame was trusted across the hole: the reference field is not being used"
        );
        // There is no keyframe after frame 1 in this stream at all, so a release from the hold *must*
        // be the reference path — nothing else could have done it.
        assert_eq!(plane.stats().keyframes_in, 1);
    }

    #[test]
    fn a_keyframe_after_a_loss_releases_the_hold() {
        // The recovery path end to end, including the wire bit that makes it possible.
        let mut plane = VideoPlane::new(policy(), None, Instant::now());
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(4);
        let mut seq = 0;
        let mut datagrams = Vec::new();
        for frame_index in 1..=40u64 {
            let (_, d) = packetizer
                .fragment(
                    x_transport::FrameMeta {
                        frame_index,
                        target_timestamp_us: frame_index * 11_111,
                        // Keyframes at 1 and 30, so a hold can begin after 1 and end at 30.
                        is_keyframe: frame_index == 1 || frame_index == 30,
                        key_epoch: 0,
                        reference_frame: 0,
                    },
                    &bytes,
                    &mut seq,
                    None,
                )
                .unwrap();
            // Drop the whole of frame 5.
            if frame_index != 5 {
                datagrams.extend(d);
            }
        }

        let mut source = ReplaySource::new(datagrams);
        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);
        let events = plane.release(now + Duration::from_millis(50));

        // Frame 5 never arrives at all, so it is released as unreconstructable and held. Every
        // frame after it inherits the broken chain and is held too — until the keyframe.
        let held = events
            .iter()
            .filter(|e| matches!(e, PlaneEvent::Held { .. }))
            .count();
        assert!(
            held >= 20,
            "the broken chain did not hold the frames behind it ({held} held): {events:?}"
        );

        // Frame 30 is a keyframe and must come out as presentable, ending the hold.
        let keyframe_presented = events.iter().any(|e| {
            matches!(
                e,
                PlaneEvent::Present {
                    frame_index: 30,
                    ..
                }
            )
        });
        assert!(
            keyframe_presented,
            "the keyframe did not release the hold — the wire bit or the gate is wrong: {events:?}"
        );

        // Nothing between 6 and 29 may be shown: the chain is broken until the keyframe at 30.
        // Frames from 30 on are legitimately presentable, which is the other half of the claim.
        for event in &events {
            if let PlaneEvent::Present { frame_index, .. } = event {
                assert!(
                    !(6..30).contains(frame_index),
                    "frame {frame_index} was presented while the chain was broken"
                );
            }
        }
    }

    #[test]
    fn loss_injection_is_deterministic() {
        // Reproducibility is the whole reason for a seeded source: a regression has to be a diff.
        let datagrams = stream(20, 4, ParityPolicy::Off, 30);
        let count = |seed: u64| {
            let mut source = LossySource::new(ReplaySource::new(datagrams.clone()), seed, 33);
            let mut out = Vec::new();
            while let SourceEvent::Datagram = source.recv(&mut out, Duration::ZERO) {}
            source.dropped()
        };
        assert_eq!(count(0xA11CE), count(0xA11CE), "the same seed differed");
        assert!(count(0xA11CE) > 0, "loss injection injected nothing");
        // Different seeds are allowed to agree by chance, but not to be identical always.
        assert!(
            count(0xA11CE) != count(0xB0B) || count(0xB0B) != count(0xC0DE),
            "every seed dropped the same datagrams — the generator is not being used"
        );
    }

    #[test]
    fn a_silent_link_asks_for_a_keyframe_then_resets() {
        // The ladder, on the receive path this time rather than the decoder's.
        let mut plane = VideoPlane::new(policy(), None, Instant::now());
        let now = Instant::now();

        // The plane reports on itself the moment it exists — a quarter of a second later would be a
        // quarter of a second in which the sender knows nothing about a client that may be able to
        // read far less than it thinks.
        let first = plane.release(now);
        assert!(
            matches!(first.as_slice(), [PlaneEvent::QueueDelay { .. }]),
            "a new plane does not introduce itself: {first:?}"
        );
        assert_eq!(plane.stats().keyframe_requests, 0);

        let asked = plane.release(now + crate::stall::ASK_FOR_KEYFRAME_AFTER);
        assert!(
            asked
                .iter()
                .any(|e| matches!(e, PlaneEvent::AskForKeyframe { .. })),
            "no keyframe request after the ask threshold: {asked:?}"
        );
        assert_eq!(plane.stats().keyframe_requests, 1);

        assert!(
            !plane
                .release(now + Duration::from_millis(500))
                .iter()
                .any(|e| matches!(e, PlaneEvent::AskForKeyframe { .. })),
            "it asked twice"
        );
        assert_eq!(plane.stats().keyframe_requests, 1);

        let reset = plane.release(now + crate::stall::HARD_RESET_AFTER);
        assert!(
            reset.iter().any(|e| matches!(e, PlaneEvent::Reset { .. })),
            "no reset after the reset threshold: {reset:?}"
        );
        assert_eq!(plane.stats().resets, 1);
    }

    #[test]
    fn incomplete_frames_are_nacked_rather_than_left_to_die() {
        // The reference client re-requests; so do we. A frame that is one datagram short and gets
        // it back is a frame that never needed a keyframe.
        //
        // A policy with a real hold, because that is the state a NACK exists in: a frame the
        // receiver is still holding and still missing pieces of. With no hold at all the frame is
        // released the instant it is not repairable, and there is nothing left to ask for — which
        // is correct behaviour and simply not this test.
        let hold = ReleasePolicy {
            straggler_delay: Duration::from_millis(10),
            repair_delay: Duration::from_millis(20),
            late_hold_frames: 0,
            jitter_frames: 0,
        };
        let mut plane = VideoPlane::new(hold, None, Instant::now());
        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(8);
        let mut seq = 0;
        let (_, datagrams) = packetizer
            .fragment(
                x_transport::FrameMeta {
                    frame_index: 1,
                    target_timestamp_us: 1,
                    is_keyframe: true,
                    key_epoch: 0,
                    reference_frame: 0,
                },
                &bytes,
                &mut seq,
                None,
            )
            .unwrap();

        // Deliver all but one data shard.
        let mut source = ReplaySource::new(
            datagrams
                .iter()
                .skip(1) // the first data shard is the one that never arrives
                .cloned()
                .collect(),
        );
        let now = Instant::now();
        plane.pump(&mut source, now, Duration::ZERO);

        // Nothing is released yet — the frame is incomplete — but it must be named.
        let events = plane.release(now + Duration::from_millis(1));
        let nacked = events.iter().find_map(|e| match e {
            PlaneEvent::Nack { fragments, .. } => Some(fragments.clone()),
            _ => None,
        });
        assert_eq!(
            nacked,
            Some(vec![0]),
            "the missing fragment was not named for re-request: {events:?}"
        );
    }

    /// Bind a UDP socket pair, so the source under test is a real kernel socket rather than a
    /// pretend one. The two failures this class exists to make visible — a datagram from a stranger
    /// and a datagram too big to be a fragment — are both properties of a real socket.
    /// `ConnectionError` is not `Debug`, so a helper rather than `unwrap`.
    fn socket_from(socket: std::net::UdpSocket) -> MediaSocket {
        match MediaSocket::from_std(socket) {
            Ok(socket) => socket,
            Err(e) => panic!("wrapping a bound socket: {e}"),
        }
    }

    fn socket_pair() -> (std::net::UdpSocket, std::net::UdpSocket) {
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        (receiver, sender)
    }

    #[test]
    fn a_socket_that_has_nothing_returns_a_timeout_rather_than_blocking() {
        let (receiver, _sender) = socket_pair();
        let mut source = socket_from(receiver);

        let mut out = Vec::new();
        assert_eq!(
            source.recv(&mut out, Duration::from_millis(5)),
            SourceEvent::Timeout
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_datagram_from_another_host_is_counted_and_not_handed_over() {
        let (receiver, sender) = socket_pair();
        let receiver_addr = receiver.local_addr().unwrap();
        // Deliberately *not* the sender's address: everything the sender sends is a stranger's.
        let peer: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();

        let mut socket = socket_from(receiver);
        socket.accept_only_from(peer);
        let mut source = socket;
        sender.send_to(b"not the streamer", receiver_addr).unwrap();

        let mut out = Vec::new();
        assert_eq!(
            source.recv(&mut out, Duration::from_millis(20)),
            SourceEvent::Timeout,
            "a datagram from the wrong host must not be handed to the receiver as a fragment"
        );
        assert!(out.is_empty());

        // The count is what matters: it is the only record that something else is talking to us.
        assert_eq!(
            source.datagrams_from_elsewhere(),
            1,
            "the foreign datagram was not counted"
        );
    }

    #[test]
    fn a_datagram_larger_than_the_media_mtu_is_refused_rather_than_truncated_to_fit() {
        let (receiver, sender) = socket_pair();
        let receiver_addr = receiver.local_addr().unwrap();
        let mut source = socket_from(receiver);

        // A datagram exactly as large as the read buffer is indistinguishable from a truncated
        // one, so it is refused. Treating it as whole is how a routing problem becomes a mystery.
        // The size and the refusal both live in `alvr_sockets::media`, which tests them.
        let oversized = vec![0u8; 4096];
        sender.send_to(&oversized, receiver_addr).unwrap();

        let mut out = Vec::new();
        assert_eq!(
            source.recv(&mut out, Duration::from_millis(20)),
            SourceEvent::Timeout
        );
        assert!(out.is_empty());
        assert_eq!(
            source.datagrams_oversized(),
            1,
            "an oversized datagram was not counted"
        );
    }

    /// The whole point of the socket-backed receiver: a real datagram, sent over a real socket,
    /// comes back out as a decoded frame with its identity intact.
    #[test]
    fn a_frame_sent_over_a_real_socket_comes_out_as_a_decode_action() {
        let (receiver, sender) = socket_pair();
        let peer = receiver.local_addr().unwrap();

        let packetizer = Packetizer::new(MTU, ParityPolicy::Off);
        let bytes = payload(3);
        let (_, datagrams) = packetizer
            .fragment(
                x_transport::FrameMeta {
                    frame_index: 7,
                    target_timestamp_us: 123_456,
                    is_keyframe: true,
                    key_epoch: 0,
                    reference_frame: 0,
                },
                &bytes,
                &mut 0,
                None,
            )
            .unwrap();
        for datagram in &datagrams {
            sender.send_to(datagram, peer).unwrap();
        }

        let now = Instant::now();
        // A generous deadline, because the property under test is the opposite one: a **complete**
        // frame must come out at once, not at its deadline. Waiting for a deadline that is not
        // telling us anything is latency added for nothing.
        let policy = ReleasePolicy {
            straggler_delay: Duration::ZERO,
            repair_delay: Duration::ZERO,
            late_hold_frames: 0,
            jitter_frames: 0,
        };
        let mut socket = socket_from(receiver);
        socket.accept_only_from(sender.local_addr().unwrap());
        let mut plane = MediaPlaneReceiver::new(socket, policy, None, now);

        let mut trace = LatencyTrace::new(64);
        let (actions, open) = plane.poll(&mut trace, now, Duration::ZERO);
        assert!(open, "a live socket must report itself open");

        let (frame_index, payload) = actions
            .iter()
            .find_map(|action| match action {
                MediaPlaneAction::Decode {
                    frame_index,
                    payload,
                    ..
                } => Some((*frame_index, payload.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("a complete frame did not come out at once: {actions:?}"));

        assert_eq!(frame_index, 7, "the frame's identity was lost on the way");
        assert_eq!(
            payload, bytes,
            "the payload that reached the decoder is not the payload that was sent"
        );
        assert_eq!(plane.stats().frames_presented, 1);
        assert_eq!(plane.stats().frames_held, 0);
        // The instrument runs in the real path: the frame that came out is in the trace, under the
        // same index, with the server's own target time beside it.
        assert_eq!(trace.len(), 1);
        assert_eq!(trace.frame_index_at(0), Some(7));
    }

    /// **The switch, end to end.** A real `MediaSender` on one side of a real socket, the client's
    /// `MediaPlaneReceiver` on the other, and the frame that comes out has to be the frame that went
    /// in — header and all.
    ///
    /// This exists because the switch introduced framing that nothing exercised: the per-frame
    /// header is now serialised *in front of* the NAL and travels on the same datagrams, and if the
    /// split is wrong the client hands the decoder a NAL with a header glued to the front of it.
    /// That is a decoder that either fails or produces a plausible wrong picture, and neither of
    /// those is a thing to find out on a device.
    #[test]
    fn a_frame_the_server_sends_arrives_with_its_header_and_its_bytes() {
        use alvr_common::ViewParams;
        use alvr_packets::VideoPacketHeader;

        /// `ConnectionError` is not `Debug`, so an assertion helper rather than `unwrap`.
        fn ok<T>(result: alvr_common::ConResult<T>, what: &str) -> T {
            match result {
                Ok(value) => value,
                Err(e) => panic!("{what}: {e}"),
            }
        }

        // Only the receiver's socket is used: the sending end is a `MediaSocket::connect_to`, which
        // is the shape the server actually has.
        let (receiver_socket, _unused) = socket_pair();
        let receiver_addr = receiver_socket.local_addr().unwrap();
        let mut sender_side = ok(MediaSocket::connect_to(receiver_addr, None), "connect");
        let receiver_side = socket_from(receiver_socket);
        // Deliberately *not* pinned to `sender_side.local_addr()`: the sender bound an unspecified
        // address, so it reports `0.0.0.0:port` while its datagrams arrive from `127.0.0.1:port`.
        // Pinning to the reported address rejects every datagram as foreign — which is exactly what
        // this test did the first time it ran, and why `MediaSocket::connect_to` now documents it.

        let interval = Duration::from_millis(11);
        let schedule = x_transport::KeySchedule::with_frames_per_key([42u8; 32], u64::MAX);
        let policy = ReleasePolicy {
            straggler_delay: Duration::ZERO,
            repair_delay: Duration::from_millis(30),
            late_hold_frames: 0,
            jitter_frames: 0,
        };

        let mut sender = x_transport::MediaSender::new(
            x_transport::SenderConfig::matching_policy(
                MTU,
                ParityPolicy::Ratio { fraction: 0.05 },
                &policy,
                interval,
            ),
            x_transport::PacerConfig::for_rate(300_000_000, interval),
            interval,
            Some(schedule.clone()),
        );

        let header = VideoPacketHeader {
            frame_index: 7,
            timestamp: Duration::from_millis(1_234),
            global_view_params: [ViewParams::DUMMY; 2],
            foveation_center_shifts: Some([[0.25, 0.5], [0.75, 0.5]]),
            is_idr: true,
            reference_frame: 0,
        };
        let nal: Vec<u8> = vec![0, 0, 0, 1, 0x26, 0x01, 0xde, 0xad, 0xbe, 0xef];

        // Exactly what the server does: the header in front of the NAL, one frame.
        let mut frame_bytes =
            bincode::serde::encode_to_vec(&header, bincode::config::standard()).unwrap();
        frame_bytes.extend_from_slice(&nal);

        let sent = sender.send_frame(
            &mut sender_side,
            x_transport::FrameMeta {
                frame_index: header.frame_index,
                target_timestamp_us: header.timestamp.as_micros() as u64,
                is_keyframe: header.is_idr,
                key_epoch: 0,
                reference_frame: 0,
            },
            &frame_bytes,
            Duration::ZERO,
        );
        assert_eq!(sent.refused, 0, "the frame was not sent");

        let now = Instant::now();
        let mut plane = MediaPlaneReceiver::new(receiver_side, policy, Some(schedule), now);
        let mut trace = LatencyTrace::new(64);

        let mut decoded = None;
        for _ in 0..50 {
            let (actions, open) = plane.poll(&mut trace, Instant::now(), Duration::from_millis(5));
            assert!(open);
            for action in actions {
                if let MediaPlaneAction::Decode { payload, .. } = action {
                    decoded = Some(payload);
                }
            }
            if decoded.is_some() {
                break;
            }
        }

        let payload = decoded.unwrap_or_else(|| {
            panic!(
                "the frame never came out: {} (socket: {} received, {} from elsewhere, {} oversized)",
                plane.stats().summary(),
                plane.datagrams_received(),
                plane.datagrams_from_elsewhere(),
                plane.oversized_datagrams()
            )
        });
        let (decoded_header, header_len) =
            bincode::serde::decode_from_slice::<VideoPacketHeader, _>(
                &payload,
                bincode::config::standard(),
            )
            .expect("the header in front of the frame did not decode");

        assert_eq!(decoded_header.frame_index, 7);
        assert_eq!(decoded_header.timestamp, Duration::from_millis(1_234));
        assert!(decoded_header.is_idr, "the keyframe flag was lost");
        assert_eq!(
            decoded_header.foveation_center_shifts,
            Some([[0.25, 0.5], [0.75, 0.5]]),
            "the per-frame metadata did not survive the trip"
        );
        assert_eq!(
            &payload[header_len..],
            &nal[..],
            "the bytes after the header are not the NAL that was sent"
        );
    }

    #[test]
    fn the_summary_names_the_numbers_that_matter() {
        let stats = PlaneStats {
            datagrams_received: 1000,
            datagrams_dropped_by_source: 15,
            frames_presented: 300,
            frames_abandoned: 4,
            ..Default::default()
        };
        let line = stats.summary();
        for needle in ["1000 datagrams in", "15 dropped by source", "4 abandoned"] {
            assert!(
                line.contains(needle),
                "{line:?} does not mention {needle:?}"
            );
        }
    }
}
