//! The sending half of the media plane.
//!
//! Everything here has a counterpart on the receiving side, and the point of the module is that
//! they now agree: FEC so a lost datagram is repaired rather than fatal, per-datagram AEAD under a
//! rotating key, frame identity and the keyframe bit on every shard, pacing that spreads a frame
//! over its own slot instead of firing it as a microburst, and — the half that did not exist at all
//! — a **retransmit cache** so the client's `Nack` has something to act on.
//!
//! ## The rule this module exists to enforce
//!
//! *A repair that cannot arrive in time must not be sent.* Retransmitting a fragment for a frame
//! whose period has passed does not fix anything: it spends bandwidth, it arrives late enough to be
//! discarded by the receiver's release policy, and it makes the loss statistics look better than the
//! picture does. So the cache is **deadline-aware** — a NACK for a frame that is expired, or that is
//! closer to its deadline than one round trip, is refused and counted, and the client's own stall
//! ladder is what turns that into a keyframe request.
//!
//! ## Retransmission sends the *same bytes*
//!
//! The cache holds the sealed datagrams, and a repair re-sends one byte-for-byte. The alternative —
//! re-sealing with [`Flags::RETRANSMIT`](crate::wire::Flags::RETRANSMIT) set — would put the same
//! `(frame_index, fragment_index)` nonce under one key twice with **different associated data**.
//! That specific case is not a keystream break (the plaintext is identical), but it destroys the
//! one-line invariant the crypto module is built on, and it buys nothing but a log counter: the
//! receiver already knows a frame was repaired, because [`FrameOutcome::Recovered`] says so and
//! duplicates are counted separately. The flag stays for a future where the original send and the
//! repair are different code paths.
//!
//! [`FrameOutcome::Recovered`]: crate::receiver::FrameOutcome::Recovered

use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use crate::{
    adaptive::AdaptiveParity,
    crypto::{KeySchedule, MediaCipher},
    datagram::DatagramSink,
    feedback::Feedback,
    pacer::{Pacer, PacerConfig},
    packetizer::{FrameLayout, FrameMeta, Packetizer, ParityPolicy},
};

/// How the sender behaves. Everything here is a fact about the link or about the client's own
/// policy, not a tuning knob — the one exception is noted.
#[derive(Debug, Clone, Copy)]
pub struct SenderConfig {
    pub mtu: usize,
    pub parity: ParityPolicy,
    /// How long after a frame's target time the client will still accept a repair.
    ///
    /// **Must match the receiver's `ReleasePolicy`**: a sender that believes it has longer than the
    /// client does will spend bandwidth on repairs the client throws away, and one that believes it
    /// has less will refuse repairs that would have worked. `ReleasePolicy::new(jitter_frames,
    /// interval)` has a deadline of `(jitter_frames + 1) × interval`, and that is the default here.
    pub repair_window: Duration,
    /// How many frames of sealed datagrams to keep for repair. A frame at 300 Mbps is ~400 KB, so
    /// this is the cache's memory bound: 16 frames is about 6 MB and 180 ms of stream, which is
    /// several times any plausible round trip.
    pub cached_frames: usize,
}

impl SenderConfig {
    /// A configuration whose repair window matches the receiver's release policy for the same link.
    pub fn matching_receiver(
        mtu: usize,
        parity: ParityPolicy,
        jitter_frames: u16,
        frame_interval: Duration,
    ) -> Self {
        Self {
            mtu,
            parity,
            repair_window: frame_interval * (jitter_frames as u32 + 1),
            cached_frames: 16,
        }
    }
}

/// What happened to one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSend {
    pub frame_index: u64,
    /// Datagrams handed to the sink.
    pub datagrams: usize,
    pub parity_datagrams: usize,
    pub bytes: usize,
    /// Datagrams the sink refused. Non-zero means the caller is not honouring [`Self::paced_wait`].
    pub refused: usize,
    /// How long the caller should wait before sending the next frame's first datagram.
    ///
    /// This is the pacer's accumulated debt over the frame, so sleeping it gives average-rate
    /// pacing with one frame's worth of burst — which is exactly what
    /// [`PacerConfig::for_rate`] authorises and no more. **A library that sleeps is a library that
    /// cannot be tested against a clock**, which is why this is returned rather than waited out.
    pub paced_wait: Duration,
    /// True when `paced_wait` exceeds one frame interval: the configured rate cannot carry the
    /// stream, and the bitrate controller is the thing that has to hear about it.
    pub over_budget: bool,
    pub layout: FrameLayout,
}

/// Why a repair was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairRefusal {
    /// The frame's period has passed. Nothing about this frame can help any more.
    Expired,
    /// The frame is still current, but a round trip would land it too late to be shown. Refusing
    /// this is the difference between a repair and a late frame.
    WouldArriveLate,
    /// Too old to still be cached. The cache is bounded, and this is the cost of that.
    Evicted,
}

/// What the sender did with one feedback message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackOutcome {
    /// Fragments were re-sent. `datagrams` is how many actually went.
    Repaired { frame_index: u64, datagrams: usize },
    /// A repair was asked for and refused, with the reason. The caller should not paper over this:
    /// a refused repair for a frame the client is still holding is a frame it will lose.
    RepairRefused {
        frame_index: u64,
        reason: RepairRefusal,
    },
    /// The encoder must produce a keyframe. Nothing else can repair a broken reference chain.
    KeyframeRequired { newest_frame: u64 },
    /// The client is rebuilding. The stream resumes from the newest frame it actually **showed**,
    /// which is not the newest frame sent.
    Resume { last_presented: u64 },
    /// Authentic and nothing to do — a NACK for a frame that was never ours, or a repair window
    /// that closed between the ask and its arrival.
    Ignored,
}

/// Counters. Same rule as everywhere else in this tree: a path that can lose a datagram has a
/// counter on it, or the loss becomes somebody else's bug report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SenderStats {
    pub frames_sent: u64,
    pub datagrams_sent: u64,
    pub parity_datagrams: u64,
    pub bytes_sent: u64,
    /// Frames the sink refused outright. Every one is a frame the client will have to recover
    /// without, so this must be zero.
    pub frames_refused: u64,
    pub retransmit_requests: u64,
    pub retransmitted_datagrams: u64,
    pub repairs_refused_expired: u64,
    pub repairs_refused_late: u64,
    pub repairs_refused_evicted: u64,
    /// NACKs naming a frame this sender never sent, or has long since forgotten.
    pub repairs_refused_unknown: u64,
    pub keyframes_required: u64,
    pub stream_resets: u64,
    /// Frames whose pacing debt exceeded one frame interval.
    pub over_budget_frames: u64,
}

impl SenderStats {
    /// The fraction of sent datagrams that were parity rather than data.
    pub fn overhead_fraction(&self) -> f64 {
        if self.datagrams_sent == 0 {
            0.0
        } else {
            self.parity_datagrams as f64 / self.datagrams_sent as f64
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "media sender: {} frames ({} datagrams, {} parity = {:.1} % overhead), {} refused by \
             the sink, {} repair request(s) answered with {} datagram(s), {} repair(s) refused \
             ({} expired, {} too late, {} evicted, {} unknown), {} keyframe request(s), {} reset(s), \
             {} frame(s) over budget",
            self.frames_sent,
            self.datagrams_sent,
            self.parity_datagrams,
            self.overhead_fraction() * 100.0,
            self.frames_refused,
            self.retransmit_requests,
            self.retransmitted_datagrams,
            self.repairs_refused_expired
                + self.repairs_refused_late
                + self.repairs_refused_evicted
                + self.repairs_refused_unknown,
            self.repairs_refused_expired,
            self.repairs_refused_late,
            self.repairs_refused_evicted,
            self.repairs_refused_unknown,
            self.keyframes_required,
            self.stream_resets,
            self.over_budget_frames,
        )
    }
}

/// One frame's datagrams, kept for repair.
struct CachedFrame {
    /// Indexed by fragment index, exactly as they went on the wire. Re-sent byte-for-byte.
    datagrams: Vec<Vec<u8>>,
    /// The instant after which a repair cannot help.
    repair_until: Duration,
    is_keyframe: bool,
}

/// Which key seals which frame.
#[derive(Debug)]
struct SenderCrypto {
    schedule: KeySchedule,
    epoch: u16,
    cipher: MediaCipher,
}

impl SenderCrypto {
    fn new(schedule: KeySchedule) -> Self {
        let epoch = 0;
        let cipher = schedule.cipher_for(epoch);
        Self {
            schedule,
            epoch,
            cipher,
        }
    }

    /// The cipher for a frame, rotating if the frame is in a new epoch.
    fn cipher_for(&mut self, frame_index: u64) -> (&MediaCipher, u16) {
        let epoch = self.schedule.epoch_for(frame_index);
        if epoch != self.epoch {
            self.epoch = epoch;
            self.cipher = self.schedule.cipher_for(epoch);
        }
        (&self.cipher, epoch)
    }
}

/// The sending half of a media session.
pub struct MediaSender {
    config: SenderConfig,
    packetizer: Packetizer,
    crypto: Option<SenderCrypto>,
    pacer: Pacer,
    cache: HashMap<u64, CachedFrame>,
    /// Insertion order, so eviction is oldest-first without scanning.
    order: VecDeque<u64>,
    rtt: Duration,
    frame_interval: Duration,
    adaptive: Option<AdaptiveParity>,
    parity: ParityPolicy,
    send_seq: u32,
    /// Set by a keyframe request; the caller takes it and asks the encoder.
    pending_keyframe: Option<u64>,
    resume_from: Option<u64>,
    stats: SenderStats,
}

impl MediaSender {
    pub fn new(
        config: SenderConfig,
        pacer_config: PacerConfig,
        frame_interval: Duration,
        crypto: Option<KeySchedule>,
    ) -> Self {
        Self {
            config,
            packetizer: Packetizer::new(config.mtu, config.parity),
            crypto: crypto.map(SenderCrypto::new),
            pacer: Pacer::new(pacer_config),
            cache: HashMap::new(),
            order: VecDeque::new(),
            rtt: Duration::from_millis(4),
            frame_interval,
            adaptive: None,
            parity: config.parity,
            send_seq: 0,
            pending_keyframe: None,
            resume_from: None,
            stats: SenderStats::default(),
        }
    }

    /// Let the FEC ratio follow the measured loss. Off by default: the bench gates a fixed ratio so
    /// that a change in the gate means a change in the link, not in a controller.
    pub fn with_adaptive_parity(mut self, adaptive: AdaptiveParity) -> Self {
        self.adaptive = Some(adaptive);
        self
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub fn rtt(&self) -> Duration {
        self.rtt
    }

    /// Feed the measured loss back into the parity controller, if one is installed.
    ///
    /// The rate is the caller's decision (see [`crate::pacer`]); so is this, because the loss
    /// measurement lives wherever the statistics do, and a sender that also measured would be two
    /// sources of truth for one number.
    pub fn observe_loss(&mut self, loss_fraction: f64) {
        if let Some(adaptive) = &mut self.adaptive {
            // The change is not consulted: the controller's *current* fraction is the answer, and
            // reading it back means a caller that observes twice sees the same ratio rather than a
            // transition that only exists once.
            adaptive.observe(loss_fraction);
            self.parity = ParityPolicy::Ratio {
                fraction: adaptive.fraction(),
            };
            self.packetizer = Packetizer::new(self.config.mtu, self.parity);
        }
    }

    pub fn parity_policy(&self) -> ParityPolicy {
        self.parity
    }

    pub fn stats(&self) -> &SenderStats {
        &self.stats
    }

    pub fn cached_frames(&self) -> usize {
        self.cache.len()
    }

    /// The newest keyframe request the client has made, if any. Taking it clears it: the caller is
    /// expected to act on it, and an un-taken request that is reported twice would encode two
    /// keyframes for one hold.
    pub fn take_keyframe_request(&mut self) -> Option<u64> {
        self.pending_keyframe.take()
    }

    /// Where a mid-session rebuild should resume from, if the client has asked for one.
    pub fn take_resume_point(&mut self) -> Option<u64> {
        self.resume_from.take()
    }

    /// Cut one frame up, seal it, pace it, and hand it to the sink.
    ///
    /// `now` and the frame's `target_timestamp_us` must be in the **same clock** — the sender's. The
    /// repair window is measured from the target, and a caller that passes two different clocks gets
    /// a cache that expires everything immediately or nothing at all.
    pub fn send_frame(
        &mut self,
        sink: &mut impl DatagramSink,
        meta: FrameMeta,
        payload: &[u8],
        now: Duration,
    ) -> FrameSend {
        self.stats.frames_sent += 1;

        let (cipher, epoch) = match &mut self.crypto {
            Some(crypto) => {
                let (cipher, epoch) = crypto.cipher_for(meta.frame_index);
                (Some(cipher), epoch)
            }
            None => (None, 0),
        };

        let meta = FrameMeta {
            key_epoch: epoch,
            ..meta
        };

        let (layout, datagrams) =
            match self
                .packetizer
                .fragment(meta, payload, &mut self.send_seq, cipher)
            {
                Ok(built) => built,
                Err(e) => {
                    // A frame that cannot be cut up is a frame that does not exist. Counted as refused
                    // rather than returned as an error so a caller in a send loop has one thing to
                    // check: `stats.frames_refused`.
                    log::warn!(
                        "media sender: frame {} could not be packetised: {e}",
                        meta.frame_index
                    );
                    self.stats.frames_refused += 1;
                    return FrameSend {
                        frame_index: meta.frame_index,
                        datagrams: 0,
                        parity_datagrams: 0,
                        bytes: 0,
                        refused: 0,
                        paced_wait: Duration::ZERO,
                        over_budget: false,
                        layout: FrameLayout {
                            frame_index: meta.frame_index,
                            frame_len: payload.len() as u32,
                            data_count: 0,
                            parity_count: 0,
                            blocks: 0,
                            parity_per_block: 0,
                            datagram_len: self.config.mtu,
                        },
                    };
                }
            };

        let mut refused = 0;
        let mut bytes = 0;

        for datagram in &datagrams {
            // The pacer is consulted for every datagram even though the frame is written in one go:
            // that is what accumulates the debt the caller sleeps off, and it is what makes
            // `over_budget` mean something.
            self.pacer.schedule(datagram.len(), now);
            match sink.send(datagram) {
                Ok(()) => {
                    bytes += datagram.len();
                    self.stats.datagrams_sent += 1;
                }
                Err(_) => refused += 1,
            }
        }

        self.stats.bytes_sent += bytes as u64;
        self.stats.parity_datagrams += layout.parity_count as u64;

        if refused > 0 {
            self.stats.frames_refused += 1;
        }

        let paced_wait = self.pacer.next_send().saturating_sub(now);
        let over_budget = paced_wait > self.frame_interval;
        if over_budget {
            self.stats.over_budget_frames += 1;
        }

        self.remember(
            CachedFrame {
                datagrams,
                repair_until: target_as_duration(meta.target_timestamp_us)
                    + self.config.repair_window,
                is_keyframe: meta.is_keyframe,
            },
            meta.frame_index,
        );

        FrameSend {
            frame_index: meta.frame_index,
            datagrams: layout.data_count as usize + layout.parity_count as usize,
            parity_datagrams: layout.parity_count as usize,
            bytes,
            refused,
            paced_wait,
            over_budget,
            layout,
        }
    }

    fn remember(&mut self, frame: CachedFrame, frame_index: u64) {
        if self.config.cached_frames == 0 {
            return;
        }
        self.cache.insert(frame_index, frame);
        self.order.push_back(frame_index);

        while self.order.len() > self.config.cached_frames {
            if let Some(oldest) = self.order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
    }

    /// Act on one authenticated feedback message.
    pub fn on_feedback(
        &mut self,
        sink: &mut impl DatagramSink,
        feedback: &Feedback,
        now: Duration,
    ) -> FeedbackOutcome {
        match feedback {
            Feedback::Nack {
                frame_index,
                fragments,
            } => {
                self.stats.retransmit_requests += 1;
                self.repair(sink, *frame_index, fragments, now)
            }
            Feedback::RequestKeyframe { newest_frame } => {
                self.stats.keyframes_required += 1;
                self.pending_keyframe = Some(*newest_frame);
                FeedbackOutcome::KeyframeRequired {
                    newest_frame: *newest_frame,
                }
            }
            Feedback::StreamReset { last_presented } => {
                self.stats.stream_resets += 1;
                self.resume_from = Some(*last_presented);
                // Frames older than the resume point can never be asked for again, and the client
                // has just told us it is rebuilding — so holding their datagrams is holding memory
                // for a repair that cannot happen.
                self.cache.retain(|index, _| *index > *last_presented);
                self.order.retain(|index| *index > *last_presented);
                // A fresh burst permit is legitimate here: the stream is starting again, and
                // pacing off a debt from before the reset would delay its first frame.
                self.pacer.reset();
                FeedbackOutcome::Resume {
                    last_presented: *last_presented,
                }
            }
        }
    }

    fn repair(
        &mut self,
        sink: &mut impl DatagramSink,
        frame_index: u64,
        fragments: &[u16],
        now: Duration,
    ) -> FeedbackOutcome {
        let Some(frame) = self.cache.get(&frame_index) else {
            self.stats.repairs_refused_unknown += 1;
            return FeedbackOutcome::RepairRefused {
                frame_index,
                reason: RepairRefusal::Evicted,
            };
        };

        // The deadline rule, before anything is sent.
        if now >= frame.repair_until {
            self.stats.repairs_refused_expired += 1;
            return FeedbackOutcome::RepairRefused {
                frame_index,
                reason: RepairRefusal::Expired,
            };
        }
        if now + self.rtt >= frame.repair_until {
            // It would arrive after the client has already given up on the frame. Sending it
            // spends bandwidth on a frame that cannot be shown, and makes the loss statistics look
            // better than the picture does.
            self.stats.repairs_refused_late += 1;
            return FeedbackOutcome::RepairRefused {
                frame_index,
                reason: RepairRefusal::WouldArriveLate,
            };
        }

        let mut sent = 0;
        for fragment in fragments {
            let Some(datagram) = frame.datagrams.get(*fragment as usize) else {
                // A fragment index past the end of the frame: a corrupt or hostile NACK. Skipped
                // rather than fatal, and the frame is not counted as repaired.
                continue;
            };
            self.pacer.schedule(datagram.len(), now);
            if sink.send(datagram).is_ok() {
                sent += 1;
                self.stats.retransmitted_datagrams += 1;
                self.stats.datagrams_sent += 1;
                self.stats.bytes_sent += datagram.len() as u64;
            }
        }

        FeedbackOutcome::Repaired {
            frame_index,
            datagrams: sent,
        }
    }

    /// Whether a frame is still repairable, for a caller deciding whether to answer a NACK at all.
    pub fn can_repair(&self, frame_index: u64, now: Duration) -> bool {
        self.cache
            .get(&frame_index)
            .is_some_and(|frame| now < frame.repair_until && now + self.rtt < frame.repair_until)
    }

    /// Whether a frame the sender is holding was a keyframe. The receiver's trust gate needs the
    /// client to know; the sender needs it to answer "is the stream currently keyframe-backed".
    pub fn cached_is_keyframe(&self, frame_index: u64) -> Option<bool> {
        self.cache.get(&frame_index).map(|frame| frame.is_keyframe)
    }

    /// How many parity shards a frame of this payload would carry, for a caller sizing a budget.
    pub fn parity_for(&self, payload_len: usize) -> usize {
        let data_count = payload_len.div_ceil(self.packetizer.shard_len()).max(1);
        self.parity.parity_for(data_count)
    }

    /// Data shards in this frame, for the same reason.
    pub fn data_shards_for(&self, payload_len: usize) -> usize {
        payload_len.div_ceil(self.packetizer.shard_len()).max(1)
    }
}

/// A frame's target time, as an instant on the sender's clock.
///
/// The wire carries microseconds since the session's epoch, and a frame's repair window is measured
/// from that. Saturating rather than wrapping: a target far in the future is a clock bug, and
/// treating it as "very far away" makes the cache keep the frame rather than expire it instantly.
fn target_as_duration(target_timestamp_us: u64) -> Duration {
    Duration::from_micros(target_timestamp_us)
}

impl std::fmt::Debug for MediaSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSender")
            .field("mtu", &self.config.mtu)
            .field("parity", &self.parity)
            .field("cached_frames", &self.cache.len())
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crypto::KEY_LEN, datagram::Collector};

    const MTU: usize = 1400;

    fn interval() -> Duration {
        Duration::from_micros(11_111)
    }

    fn config(parity: ParityPolicy) -> SenderConfig {
        SenderConfig::matching_receiver(MTU, parity, 1, interval())
    }

    fn sender(parity: ParityPolicy) -> MediaSender {
        MediaSender::new(
            config(parity),
            PacerConfig::for_rate(300_000_000, interval()),
            interval(),
            None,
        )
    }

    fn payload(shards: usize) -> Vec<u8> {
        let shard = MTU - crate::wire::HEADER_LEN;
        (0..shards * shard).map(|i| (i % 251) as u8).collect()
    }

    fn meta(frame_index: u64, target_us: u64) -> FrameMeta {
        FrameMeta {
            frame_index,
            target_timestamp_us: target_us,
            is_keyframe: frame_index == 1,
            key_epoch: 0,
        }
    }

    #[test]
    fn a_frame_is_cut_up_sealed_and_counted() {
        let mut sender = sender(ParityPolicy::Ratio { fraction: 0.05 });
        let mut sink = Collector::new();
        let bytes = payload(20);

        let sent = sender.send_frame(&mut sink, meta(1, 11_111), &bytes, Duration::ZERO);

        assert_eq!(sent.datagrams, 21, "20 data shards plus 5 % parity");
        assert_eq!(sent.parity_datagrams, 1);
        assert_eq!(sink.len(), 21);
        assert_eq!(sent.refused, 0);
        assert_eq!(sender.stats().frames_sent, 1);
        assert_eq!(sender.stats().datagrams_sent, 21);
    }

    #[test]
    fn a_refused_datagram_is_counted_and_ids_the_frame_it_lost() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        sink.fail_next(3);

        let sent = sender.send_frame(&mut sink, meta(1, 11_111), &payload(10), Duration::ZERO);

        assert_eq!(sent.refused, 3);
        assert_eq!(sender.stats().frames_refused, 1);
        assert_eq!(
            sender.stats().datagrams_sent,
            7,
            "only the datagrams that went are counted as sent"
        );
    }

    // -- the deadline rule ---------------------------------------------------------------------

    #[test]
    fn a_repair_inside_the_window_is_resent_byte_for_byte() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        let sent = sender.send_frame(&mut sink, meta(5, 100_000), &payload(4), Duration::ZERO);
        assert_eq!(sent.datagrams, 4);

        let original = sink.take();
        let mut repairs = Collector::new();
        let outcome = sender.on_feedback(
            &mut repairs,
            &Feedback::Nack {
                frame_index: 5,
                fragments: vec![1, 2],
            },
            Duration::from_micros(101_000),
        );

        assert_eq!(
            outcome,
            FeedbackOutcome::Repaired {
                frame_index: 5,
                datagrams: 2
            }
        );
        assert_eq!(repairs.len(), 2);
        assert_eq!(repairs.datagrams()[0], original[1]);
        assert_eq!(repairs.datagrams()[1], original[2]);
        assert_eq!(sender.stats().retransmitted_datagrams, 2);
    }

    #[test]
    fn a_repair_for_a_frame_whose_period_has_passed_is_refused_rather_than_sent() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        // Target 100 ms, window 2 frame intervals (~22 ms).
        sender.send_frame(&mut sink, meta(9, 100_000), &payload(4), Duration::ZERO);
        let mut repairs = Collector::new();

        let outcome = sender.on_feedback(
            &mut repairs,
            &Feedback::Nack {
                frame_index: 9,
                fragments: vec![0],
            },
            Duration::from_millis(200),
        );

        assert_eq!(
            outcome,
            FeedbackOutcome::RepairRefused {
                frame_index: 9,
                reason: RepairRefusal::Expired
            }
        );
        assert!(
            repairs.is_empty(),
            "a repair that cannot arrive in time must not be sent: it spends bandwidth on a frame \
             that cannot be shown and makes the loss statistics look better than the picture does"
        );
        assert_eq!(sender.stats().repairs_refused_expired, 1);
    }

    #[test]
    fn a_repair_that_would_arrive_after_the_deadline_is_refused_even_though_the_frame_is_live() {
        let mut sender = sender(ParityPolicy::Off);
        sender.set_rtt(Duration::from_millis(10));
        let mut sink = Collector::new();
        // Target 100 ms, window = 2 * 11.111 ms = 22.222 ms, so repair_until = 122.222 ms.
        sender.send_frame(&mut sink, meta(9, 100_000), &payload(4), Duration::ZERO);

        // 115 ms: still before the deadline, but 115 + 10 > 122.222.
        let mut repairs = Collector::new();
        let outcome = sender.on_feedback(
            &mut repairs,
            &Feedback::Nack {
                frame_index: 9,
                fragments: vec![0],
            },
            Duration::from_millis(115),
        );

        assert_eq!(
            outcome,
            FeedbackOutcome::RepairRefused {
                frame_index: 9,
                reason: RepairRefusal::WouldArriveLate
            }
        );
        assert!(repairs.is_empty());
        assert_eq!(sender.stats().repairs_refused_late, 1);
        assert!(!sender.can_repair(9, Duration::from_millis(115)));
        assert!(sender.can_repair(9, Duration::from_millis(110)));
    }

    #[test]
    fn a_repair_for_a_frame_that_was_never_sent_is_ignored() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        sender.send_frame(&mut sink, meta(1, 11_111), &payload(2), Duration::ZERO);

        let outcome = sender.on_feedback(
            &mut sink,
            &Feedback::Nack {
                frame_index: 999,
                fragments: vec![0],
            },
            Duration::ZERO,
        );

        assert_eq!(
            outcome,
            FeedbackOutcome::RepairRefused {
                frame_index: 999,
                reason: RepairRefusal::Evicted
            }
        );
        assert_eq!(sender.stats().repairs_refused_unknown, 1);
    }

    #[test]
    fn a_fragment_index_past_the_end_of_the_frame_is_skipped_rather_than_fatal() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        sender.send_frame(&mut sink, meta(3, 100_000), &payload(2), Duration::ZERO);

        let mut repairs = Collector::new();
        let outcome = sender.on_feedback(
            &mut repairs,
            &Feedback::Nack {
                frame_index: 3,
                fragments: vec![0, 9_999, 1],
            },
            Duration::from_micros(101_000),
        );

        assert_eq!(
            outcome,
            FeedbackOutcome::Repaired {
                frame_index: 3,
                datagrams: 2
            },
            "the two real fragments must still be repaired"
        );
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_oldest_first() {
        let mut sender = MediaSender::new(
            SenderConfig {
                cached_frames: 3,
                ..config(ParityPolicy::Off)
            },
            PacerConfig::for_rate(300_000_000, interval()),
            interval(),
            None,
        );
        let mut sink = Collector::new();
        for frame in 1..=6u64 {
            sender.send_frame(
                &mut sink,
                meta(frame, frame * 11_111),
                &payload(1),
                Duration::ZERO,
            );
        }

        assert_eq!(sender.cached_frames(), 3);
        assert!(sender.cached_is_keyframe(4).is_some());
        assert!(
            sender.cached_is_keyframe(1).is_none(),
            "the cache must be bounded: 16 frames is 6 MB, and an unbounded one is a leak with a \
             longer name"
        );
    }

    // -- keyframes and resets ------------------------------------------------------------------

    #[test]
    fn a_keyframe_request_is_held_for_the_caller_to_act_on_exactly_once() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();

        let outcome = sender.on_feedback(
            &mut sink,
            &Feedback::RequestKeyframe { newest_frame: 40 },
            Duration::ZERO,
        );
        assert_eq!(
            outcome,
            FeedbackOutcome::KeyframeRequired { newest_frame: 40 }
        );
        assert_eq!(sender.take_keyframe_request(), Some(40));
        assert_eq!(
            sender.take_keyframe_request(),
            None,
            "an un-taken request reported twice encodes two keyframes for one hold"
        );
        assert_eq!(sender.stats().keyframes_required, 1);
    }

    #[test]
    fn a_stream_reset_resumes_from_what_the_client_showed_and_forgets_what_it_cannot() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        for frame in 1..=6u64 {
            sender.send_frame(
                &mut sink,
                meta(frame, frame * 100_000),
                &payload(1),
                Duration::ZERO,
            );
        }

        let outcome = sender.on_feedback(
            &mut sink,
            &Feedback::StreamReset { last_presented: 4 },
            Duration::ZERO,
        );

        assert_eq!(outcome, FeedbackOutcome::Resume { last_presented: 4 });
        assert_eq!(sender.take_resume_point(), Some(4));
        assert!(
            sender.cached_is_keyframe(4).is_none(),
            "frames the client has already moved past cannot be asked for again"
        );
        assert!(sender.cached_is_keyframe(5).is_some());
        assert_eq!(sender.stats().stream_resets, 1);
    }

    // -- pacing --------------------------------------------------------------------------------

    /// The budget is not the payload, and the difference is the point: 300 Mbps across 90 Hz is
    /// 416,666 bytes of *datagram* per frame, and a datagram carries 1400 bytes of which 1364 is
    /// payload. So a frame of 297 shards fits and 306 does not — and 306 is what you get by
    /// dividing the bitrate by the frame rate and forgetting the header.
    #[test]
    fn a_frame_at_the_configured_rate_is_not_reported_over_budget() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        let sent = sender.send_frame(&mut sink, meta(1, 11_111), &payload(297), Duration::ZERO);

        assert!(!sent.over_budget, "{sent:?}");
        assert!(sent.paced_wait <= interval(), "{sent:?}");
        assert_eq!(sent.bytes, 297 * MTU);
    }

    /// Parity is charged to the pacer like anything else, which is correct — it occupies the same
    /// link — and it means the FEC ratio comes straight out of the payload budget. 5 % of parity
    /// costs about 5 % of the frame size, and a client tuned to the edge of the budget loses the
    /// last frames of the stream to its own protection.
    #[test]
    fn parity_comes_out_of_the_same_budget_as_the_payload() {
        let mut unprotected = sender(ParityPolicy::Off);
        let mut protected = sender(ParityPolicy::Ratio { fraction: 0.05 });
        let mut sink = Collector::new();

        let bare =
            unprotected.send_frame(&mut sink, meta(1, 11_111), &payload(297), Duration::ZERO);
        let with_fec =
            protected.send_frame(&mut sink, meta(1, 11_111), &payload(297), Duration::ZERO);

        assert!(!bare.over_budget);
        // 16, not 15, and the difference is the striping: 297 shards are two blocks of 149, and
        // 5 % of 149 rounds *up* per block, so the frame pays 8 + 8. Per-block rounding is the
        // reason a `ceil` policy costs slightly more than its nominal ratio.
        assert_eq!(with_fec.parity_datagrams, 16);
        assert!(
            with_fec.bytes > bare.bytes,
            "parity must cost bytes on the same link"
        );
        assert!(
            with_fec.over_budget,
            "the same payload plus 5 % parity no longer fits the same budget: {with_fec:?}"
        );
    }

    #[test]
    fn a_frame_past_the_rate_is_reported_over_budget_rather_than_silently_spent() {
        let mut sender = sender(ParityPolicy::Off);
        let mut sink = Collector::new();
        // Twice the budget: two frames' worth in one slot.
        let sent = sender.send_frame(&mut sink, meta(1, 11_111), &payload(594), Duration::ZERO);

        assert!(
            sent.over_budget,
            "the configured rate cannot carry this stream, and the bitrate controller is the thing \
             that has to hear about it: {sent:?}"
        );
        assert!(sent.paced_wait > interval());
        assert_eq!(sender.stats().over_budget_frames, 1);
    }

    // -- encryption ----------------------------------------------------------------------------

    #[test]
    fn consecutive_frames_are_sealed_under_different_keys_when_the_epoch_moves() {
        let schedule = KeySchedule::with_frames_per_key([4u8; KEY_LEN], 2);
        let mut sender = MediaSender::new(
            config(ParityPolicy::Off),
            PacerConfig::for_rate(300_000_000, interval()),
            interval(),
            Some(schedule.clone()),
        );
        let mut sink = Collector::new();

        for frame in 1..=4u64 {
            sender.send_frame(
                &mut sink,
                meta(frame, frame * 11_111),
                &payload(1),
                Duration::ZERO,
            );
        }

        // Decode each datagram with the ring the *receiver* would build, and check the epochs came
        // out where the schedule says.
        let mut ring = crate::KeyRing::new(schedule);
        let mut seen = Vec::new();
        for datagram in sink.datagrams() {
            let (header, body) = crate::FragmentHeader::decode(datagram).unwrap();
            seen.push(header.key_epoch);
            let cipher = ring
                .cipher_for(header.key_epoch)
                .expect("epoch in the ring");
            assert!(
                cipher
                    .open(
                        header.frame_index,
                        header.fragment_index,
                        &datagram[..crate::HEADER_LEN],
                        body
                    )
                    .is_ok(),
                "frame {} (epoch {}) did not open",
                header.frame_index,
                header.key_epoch
            );
        }

        assert_eq!(seen, [0, 1, 1, 2], "epochs did not follow the frame index");
    }

    #[test]
    fn the_summary_names_every_counter() {
        let mut sender = sender(ParityPolicy::Ratio { fraction: 0.05 });
        let mut sink = Collector::new();
        sink.fail_next(1);
        sender.send_frame(&mut sink, meta(1, 100_000), &payload(20), Duration::ZERO);
        sender.on_feedback(
            &mut sink,
            &Feedback::Nack {
                frame_index: 1,
                fragments: vec![0],
            },
            Duration::from_micros(101_000),
        );
        sender.on_feedback(
            &mut sink,
            &Feedback::RequestKeyframe { newest_frame: 1 },
            Duration::ZERO,
        );

        let line = sender.stats().summary();
        for needle in [
            "frames",
            "parity",
            "refused by the sink",
            "repair request(s)",
            "keyframe request(s)",
            "over budget",
        ] {
            assert!(
                line.contains(needle),
                "{line:?} does not mention {needle:?}"
            );
        }
    }
}
