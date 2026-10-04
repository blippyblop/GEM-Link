use alvr_common::SlidingWindowAverage;
use alvr_events::BitrateDirectives;
use alvr_session::{
    BitrateAdaptiveFramerateConfig, BitrateConfig, BitrateMode, settings_schema::Switch,
};
use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

const UPDATE_INTERVAL: Duration = Duration::from_secs(1);

/// How far behind the client has to report being, in frame intervals, before the ladder gives up
/// frame rate rather than waiting.
///
/// One frame: a client that is a frame behind is showing frames late, which is the latency rung and
/// costs nothing but the delay itself. Past that it is not catching up while being fed at this rate.
const LADDER_FRAMERATE_FROM: f32 = 1.0;

/// The share of a frame's declared shards that may go missing before the ladder gives up frame rate
/// on that evidence. Two percent is already a hole in most frames; above it, one step per two percent.
const LADDER_MISSING_FROM_PERMILLE: u32 = 20;

/// Below this share of a frame missing, the ladder gives a frame rate back. Well under the threshold
/// that takes it away, so the two do not meet in the middle and oscillate.
const LADDER_MISSING_RECOVER_PERMILLE: u32 = 5;

/// The frame the bootstrap is willing to send, in bytes: about eighteen datagrams, which at the
/// measured read rate of this rig is under a tenth of a second of reading.
const BOOTSTRAP_FRAME_BYTES: usize = 24_000;

/// How much smaller each bootstrap retry asks for.
const BOOTSTRAP_SHRINK: f64 = 0.6;

/// How much more than the budget the sender must have offered before a reading is treated as evidence
/// about the client. A little above one: the budget is 70 % of a previous measurement, so offering
/// exactly the budget is already offering more than the client read last time.
const READ_INFORMATIVE_MARGIN: f64 = 1.0;

/// How fast the read-rate high-water mark decays when the client reports less. See
/// [`BitrateManager::report_client_read_rate`].
const READ_CEILING_DECAY: f64 = 0.95;

/// The lowest the read-rate ceiling may fall to. See `report_client_read_rate`: below this the stream
/// is throttling itself, not following the client.
const MIN_READ_CEILING_PER_SEC: u32 = 80;

/// How much of the measured read rate the sender plans to use. See
/// [`BitrateManager::delivery_budget_per_sec`]: under one, because the sensor is a lower bound.
const DELIVERY_BUDGET_FRACTION: f64 = 0.7;

/// The smallest frame worth sending, in bytes. Below it a frame is a few shards of nothing, and the
/// honest lever is fewer frames per second rather than an unreadable picture.
const MIN_FRAME_BYTES: f64 = 6_000.0;

/// The share of a frame missing at which the client is 'drowning' — the point at which asking it to
/// repair its own frames is asking it to add to the congestion that is losing them.
const NACK_DROWNING_PERMILLE: u32 = 100;

/// The most the frame rate is reduced by: one frame in six.
///
/// Measured, not chosen: the emulated client reads ~300 datagrams/s, so at 72 Hz it can take about
/// four datagrams per frame, and a stream of 26-datagram frames is one it cannot read no matter what
/// the picture costs. Six is 12 fps at 72 Hz — the "drops to 15 fps for a few seconds" the ladder is
/// allowed to spend, and a great deal better than a hole.
const DEGRADE_MAX_FRAME_DIVISOR: u32 = 12;

/// The lowest the quality rung may take the rate, in bits per second.
///
/// An absolute floor rather than a fraction of the setting, and the first version was the latter —
/// a quarter of 30 Mbps is 7.5 Mbps, which is *above* what this client can read, so the "floor" was
/// the thing keeping the sender from ever matching the receiver. Below this the stream stops being a
/// stream, and the honest thing left is to say the ladder is exhausted.
const QUALITY_FLOOR_BPS: f32 = 1.5e6;

pub struct DynamicEncoderParams {
    pub bitrate_bps: f32,
    pub framerate: f32,
}

/// What the encoder may reference, and how much of the recent past the client has confirmed.
///
/// `valid` is false until the first acknowledgement of a session, and `0`/all-zero are then not
/// answers to anything. The mask covers the 64 frame indices at or before `newest`, bit `i` meaning
/// `newest - i` — the encoder's actual question is "was frame X decoded", and a cursor cannot answer
/// it, because a client that skipped a frame still acknowledges later ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientAck {
    pub valid: bool,
    pub newest: u64,
    pub recent_mask: u64,
}

impl ClientAck {
    /// Whether this exact frame index has been confirmed decoded.
    pub fn contains(&self, frame_index: u64) -> bool {
        if !self.valid || frame_index == 0 || frame_index > self.newest {
            return false;
        }
        let distance = self.newest - frame_index;
        if distance >= 64 {
            return false;
        }
        (self.recent_mask >> distance) & 1 != 0
    }
}

pub struct BitrateManager {
    nominal_frame_interval: Duration,
    frame_interval_average: SlidingWindowAverage<Duration>,
    // note: why packet_sizes_bits_history is a queue and not a sliding average? Because some
    // network samples will be dropped but not any packet size sample
    packet_bytes_history: VecDeque<(Duration, usize)>,
    packet_bytes_average: SlidingWindowAverage<f32>,
    network_latency_average: SlidingWindowAverage<Duration>,
    encoder_latency_average: SlidingWindowAverage<Duration>,
    decoder_latency_overstep_count: usize,
    last_frame_instant: Instant,
    last_update_instant: Instant,
    dynamic_decoder_max_bytes_per_frame: f32,
    /// What share of each frame's declared shards the client says never arrived, in tenths of a
    /// percent. See [`Self::report_client_missing`].
    client_missing_permille: Option<u16>,
    /// The rate the ladder is currently asking the encoder for, once the deficit has been applied.
    ///
    /// State, like the divisor, and for the same reason: the deficit is measured over the last thirty
    /// frames, so a rung recomputed from the newest report saws around the answer instead of settling
    /// on it. Coming down is proportional to the deficit; going back up is one step per clean report,
    /// because that is the half where being slow costs nothing.
    degrade_bitrate_bps: Option<f32>,
    /// **The delivery budget: datagrams per second the client can actually read.**
    ///
    /// One number, derived from the read-rate sensor the client reports, and everything else is solved
    /// from it: how many bytes a frame may be, how many datagrams a frame may take, how many frames
    /// per second fit, and whether a keyframe is even sendable. It replaces the proportional bitrate
    /// cap, which could not bind because it was derived from a queueing delay that a client completing
    /// nothing never produces.
    ///
    /// Seventy percent of what the client has shown it can read, because the sensor is a **lower
    /// bound**: it only ever measured a client being offered exactly what it could take. Planning at
    /// the measured rate would plan at the offered load and stay there.
    delivery_budget_per_sec: Option<f64>,
    /// What the client measured, kept for reporting: the sensor's own number, before the margin.
    read_ceiling_per_sec: u32,
    /// What the sender was offering at the last report. See `report_client_read_rate`.
    last_offered_per_sec: Option<u32>,
    /// The media plane's shape, without which a datagram budget cannot be turned into bytes: how many
    /// payload bytes a shard carries, and the fraction of extra shards the FEC adds.
    shard_bytes: usize,
    parity_ratio: f32,
    /// Whether the stream is in its bootstrap, and how big the frame it is willing to send may be.
    /// See [`BitrateManager::set_bootstrap`].
    bootstrap: bool,
    bootstrap_target_bytes: usize,
    /// The last read-rate report's age, so a budget built on a stale measurement can be distrusted.
    read_report_at: Option<Instant>,
    /// How many frames the ladder is currently dropping for every one it sends.
    ///
    /// **State, not a function of the last report.** The deficit says how much too much is being
    /// sent — `1 / (1 - missing)` times — and the answer to that is a *rate*, which has to be
    /// accumulated rather than recomputed, or the ladder saws: the client's report is of the last
    /// thirty frames, so a step that only responds to the newest one oscillates around the answer
    /// instead of converging on it.
    degrade_divisor: u32,
    /// The client's own queueing delay, as it last reported it. See
    /// [`alvr_events::BitrateDirectives::client_queue_limiter_bps`].
    ///
    /// Kept as the raw report and turned into a rate only here, because this is the only place that
    /// knows what the frame interval is — and one frame interval is the target: a client more than a
    /// frame behind is a client whose frames will be released before they have been read.
    client_queue_delay_us: Option<u32>,
    /// The bitrate last handed to the encoder. Used to avoid re-configuring it for a change too
    /// small to matter, and to report what the encoder was actually asked for.
    last_returned_bitrate_bps: Option<f32>,
    /// What the client has confirmed decoded. See [`ClientAck`].
    client_ack: ClientAck,
    previous_config: Option<BitrateConfig>,
    update_needed: bool,
}

impl BitrateManager {
    pub fn new(max_history_size: usize, initial_framerate: f32) -> Self {
        Self {
            nominal_frame_interval: Duration::from_secs_f32(1. / initial_framerate),
            frame_interval_average: SlidingWindowAverage::new(
                Duration::from_millis(16),
                max_history_size,
            ),
            packet_bytes_history: VecDeque::new(),
            packet_bytes_average: SlidingWindowAverage::new(50000.0, max_history_size),
            network_latency_average: SlidingWindowAverage::new(
                Duration::from_millis(5),
                max_history_size,
            ),
            encoder_latency_average: SlidingWindowAverage::new(
                Duration::from_millis(5),
                max_history_size,
            ),
            decoder_latency_overstep_count: 0,
            last_frame_instant: Instant::now(),
            last_update_instant: Instant::now(),
            dynamic_decoder_max_bytes_per_frame: f32::MAX,
            client_queue_delay_us: None,
            client_missing_permille: None,
            delivery_budget_per_sec: None,
            read_ceiling_per_sec: 0,
            last_offered_per_sec: None,
            read_report_at: None,
            shard_bytes: 1_360,
            parity_ratio: 0.0,
            bootstrap: false,
            bootstrap_target_bytes: BOOTSTRAP_FRAME_BYTES,
            degrade_bitrate_bps: None,
            degrade_divisor: 1,
            last_returned_bitrate_bps: None,
            client_ack: ClientAck::default(),
            previous_config: None,
            update_needed: true,
        }
    }

    // Note: This is used to calculate the framerate/frame interval. The frame present is the most
    // accurate event for this use.
    /// The client's own report of how far behind it is reading.
    ///
    /// Not a frame fact and not a server fact: it is the receiver telling the sender to send less.
    pub fn report_client_queue_delay(&mut self, micros: u32) {
        self.client_queue_delay_us = Some(micros);
    }

    /// Record what the client says it read: the sensor the whole budget is built on.
    ///
    /// The maximum over the client's window, so a burst is not averaged away. Deliberately not
    /// smoothed here as well: the client already reports a maximum over a quarter of a second, and a
    /// second smoothing would make the budget a memory of a link rather than a measurement of one.
    /// A **high-water mark that decays**, not the newest reading.
    ///
    /// The sensor is a lower bound on capacity: it can only ever measure what the sender offered. A
    /// window in which the sender happened to offer little — because the ladder had already reduced
    /// the rate, or because a stop-and-wait was in progress — reads as a slow *client*, and taking it
    /// at face value shrinks the budget, which offers less, which reads slower still. Measured on the
    /// rig: 144 datagrams/s, then 0, then a budget of 27/s, which is a collapse dressed as a
    /// measurement.
    ///
    /// So a reading below the mark is treated as a window that was starved, and the mark decays
    /// slowly instead — five percent per report, so a client that has genuinely slowed down is
    /// followed within a few seconds while a momentary stall costs nothing.
    /// `offered_per_sec` is what the sender actually put on the wire in the same window, because
    /// **a reading is only evidence about the client when the sender offered more than the client
    /// read**. A window in which the bootstrap held the stream to one frame is a window that says
    /// nothing about capacity, and letting it decay the ceiling is the same collapse in slow motion:
    /// measured, a stream that started at 222 datagrams/s settles at 18 as its own stop-and-wait
    /// teaches the budget to expect nothing.
    pub fn report_client_read_rate(&mut self, per_sec: u32, offered_per_sec: u32) {
        self.read_report_at = Some(Instant::now());
        self.last_offered_per_sec = Some(offered_per_sec);

        let informative = offered_per_sec as f64
            >= self.delivery_budget_per_sec.unwrap_or(f64::MAX) * READ_INFORMATIVE_MARGIN;
        if !informative {
            // Hold the ceiling: this window was starved by our own pacing, not by the client.
            return;
        }

        // **With a floor.** Once the ceiling reaches zero the divisor is pinned at its maximum and the
        // frame budget at its minimum, and the stream throttles itself to a few frames a second for
        // the rest of the session — measured: a run that presented 307 frames settled at `budget 0`,
        // `1 frame in 12`. A client that genuinely reads nothing is a dead client, and that is the
        // stall ladder's judgement to make, not the budget's.
        let decayed = (self.read_ceiling_per_sec as f64 * READ_CEILING_DECAY) as u32;
        self.read_ceiling_per_sec = per_sec.max(decayed).max(MIN_READ_CEILING_PER_SEC);
        if self.read_ceiling_per_sec > 0 {
            self.delivery_budget_per_sec =
                Some(self.read_ceiling_per_sec as f64 * DELIVERY_BUDGET_FRACTION);
        }
    }

    /// What the sender was offering when the client last reported, for the summary.
    pub fn last_offered_per_sec(&self) -> Option<u32> {
        self.last_offered_per_sec
    }

    /// Tell the manager the media plane's shape, so a datagram budget can become a byte budget.
    ///
    /// Not derivable here: the shard size comes from the packet size on the wire and the ratio from
    /// the FEC policy the sender is currently running, and both live with the sender.
    pub fn set_media_shape(&mut self, shard_bytes: usize, parity_ratio: f32) {
        self.shard_bytes = shard_bytes.max(1);
        self.parity_ratio = parity_ratio.max(0.0);
    }

    /// Enter or leave the **bootstrap**, in which the encoder is asked for one small intra frame at a
    /// time. See [`MediaSender::set_stop_and_wait`](x_transport::MediaSender::set_stop_and_wait) for
    /// the wire half of it; this is the half that sizes the frame.
    pub fn set_bootstrap(&mut self, on: bool) {
        self.bootstrap = on;
    }

    pub fn is_bootstrap(&self) -> bool {
        self.bootstrap
    }

    /// How many bytes the bootstrap frame may be.
    ///
    /// Default: about eighteen datagrams. A frame the client can read in under a tenth of a second at
    /// the rate it has shown — measured, a 77 KB keyframe is 64 datagrams and 220 ms of pure reading,
    /// which is why the stream never started.
    pub fn bootstrap_target_bytes(&self) -> usize {
        self.bootstrap_target_bytes
    }

    /// Ask for a smaller bootstrap frame after one was lost, and return the new size.
    ///
    /// The retry the bootstrap needs: a frame that did not arrive in the time its size implied was
    /// too big for the client, and repeating the same one repeats the failure. Shrinking is
    /// multiplicative for the same reason the FEC ratio's step is.
    pub fn shrink_bootstrap(&mut self) -> usize {
        let smaller = (self.bootstrap_target_bytes as f64 * BOOTSTRAP_SHRINK) as usize;
        self.bootstrap_target_bytes = smaller.max(MIN_FRAME_BYTES as usize);
        self.bootstrap_target_bytes
    }

    /// The delivery budget in datagrams per second, or `None` before the client has measured one.
    pub fn delivery_budget_per_sec(&self) -> Option<f64> {
        self.delivery_budget_per_sec
    }

    /// What the client measured before the margin. Zero until it has measured anything.
    pub fn read_ceiling_per_sec(&self) -> u32 {
        self.read_ceiling_per_sec
    }

    /// Record what share of each frame's declared shards the client says never arrived.
    ///
    /// The number the queueing delay cannot supply, and the live rig is the measurement: a client
    /// losing two thirds of every frame completes only the small ones, so its *drain spread* — the
    /// delay measured on frames that completed — read 21 ms, one and a half frames behind, the
    /// mildest rung of the ladder, while every frame was becoming a hole. Missing shards cannot be
    /// flattered by reading faster, because the frame declares what it should have been.
    pub fn report_client_missing(&mut self, permille: u16) {
        let permille = permille.min(1000);
        self.client_missing_permille = Some(permille);

        // The frame-rate rung, as a controller rather than a lookup table.
        //
        // What the client cannot receive is what should not be sent: if it is missing a share `m` of
        // every frame, then sending at a rate scaled by `(1 - m)` leaves it with whole frames, and
        // that step is multiplicative for the same reason the FEC ratio's is — a link that loses a
        // tenth needs a tenth more, and one that loses two thirds needs three times as much again.
        //
        // Below the threshold the need is reversed, and the divisor decays by one step per report
        // rather than jumping back: coming *out* of a degrade is the half that has to be slow, or a
        // client that has just stopped drowning is drowned by the recovery.
        if permille > LADDER_MISSING_FROM_PERMILLE as u16 {
            let missing = permille as f32 / 1000.0;
            let scale = 1.0 / (1.0 - missing).max(0.05);
            let stepped = self.degrade_divisor as f32 * scale;
            self.degrade_divisor = self
                .degrade_divisor
                .max(1)
                .max(stepped.ceil() as u32)
                .min(DEGRADE_MAX_FRAME_DIVISOR);
        } else if permille < LADDER_MISSING_RECOVER_PERMILLE as u16 {
            if self.degrade_divisor > 1 {
                self.degrade_divisor -= 1;
            }
            if let Some(bps) = &mut self.degrade_bitrate_bps {
                // A tenth back per clean report, and the state is dropped once it is no longer
                // limiting anything.
                *bps *= 1.1;
            }
        }
    }

    /// Whether the ladder is engaged at all — the flag the encoder is given so that it does not
    /// spend a keyframe on a client that cannot receive one. See [`Self::degrade_divisor`].
    pub fn is_degrading(&self) -> bool {
        self.degrade_divisor > 1
            || self.client_missing_permille.is_some_and(|m| m as u32 > NACK_DROWNING_PERMILLE)
    }

    /// Record that the client decoded a frame. See [`ClientAck`].
    ///
    /// The newest index only ever moves forward — acknowledgements arrive in order and the encoder's
    /// question is about the recent past — but the *mask* is what makes a skipped frame visible, so
    /// the bits are shifted by however far the cursor moved.
    pub fn report_client_ack(&mut self, frame_index: u64) {
        let distance = if self.client_ack.valid {
            frame_index.saturating_sub(self.client_ack.newest)
        } else {
            // First acknowledgement of the session: the history starts here, because nothing before
            // it was confirmed and nothing before it may be referenced.
            64
        };
        self.client_ack.recent_mask = if distance >= 64 {
            1
        } else {
            (self.client_ack.recent_mask << distance) | 1
        };
        self.client_ack.newest = self.client_ack.newest.max(frame_index);
        self.client_ack.valid = true;
    }

    /// What the encoder may reference. See [`ClientAck`].
    pub fn client_ack(&self) -> ClientAck {
        self.client_ack
    }

    /// The bitrate the encoder was last asked for, and the cap the client's queueing imposed.
    ///
    /// The bisect this exists for: mean encoded bytes per frame next to the number the encoder was
    /// given. Bytes unchanged means the number is not reaching it, or it is ignoring it (this
    /// session's encoder is CBR on content that never approaches the target, so it can ignore it
    /// while looking healthy); bytes changed and datagrams unchanged means packetisation.
    pub fn effective_bitrate_bps(&self) -> Option<f32> {
        self.last_returned_bitrate_bps
    }

    pub fn report_frame_present(&mut self, config: &Switch<BitrateAdaptiveFramerateConfig>) {
        let now = Instant::now();

        let interval = now - self.last_frame_instant;
        self.last_frame_instant = now;

        if let Some(config) = config.as_option() {
            let interval_ratio =
                interval.as_secs_f32() / self.frame_interval_average.get_average().as_secs_f32();

            self.frame_interval_average.submit_sample(interval);

            if interval_ratio > config.framerate_reset_threshold_multiplier
                || interval_ratio < 1.0 / config.framerate_reset_threshold_multiplier
            {
                // Clear most of the samples, keep some for stability
                self.frame_interval_average
                    .retain(NonZeroUsize::new(5).unwrap());
                self.update_needed = true;
            }
        }
    }

    pub fn report_frame_encoded(
        &mut self,
        timestamp: Duration,
        encoder_latency: Duration,
        size_bytes: usize,
    ) {
        self.encoder_latency_average.submit_sample(encoder_latency);

        self.packet_bytes_history.push_back((timestamp, size_bytes));
    }

    // decoder_latency is used to learn a suitable maximum bitrate bound to avoid decoder runaway
    // latency
    pub fn report_frame_latencies(
        &mut self,
        config: &BitrateMode,
        timestamp: Duration,
        network_latency: Duration,
        decoder_latency: Duration,
    ) {
        if network_latency.is_zero() {
            return;
        }

        while let Some(&(history_timestamp, size_bytes)) = self.packet_bytes_history.front() {
            if history_timestamp == timestamp {
                self.packet_bytes_average.submit_sample(size_bytes as f32);
                self.network_latency_average.submit_sample(network_latency);

                self.packet_bytes_history.pop_front();

                break;
            } else {
                self.packet_bytes_history.pop_front();
            }
        }

        if let BitrateMode::Adaptive {
            decoder_latency_limiter: Switch::Enabled(config),
            ..
        } = &config
        {
            if decoder_latency > Duration::from_millis(config.max_decoder_latency_ms) {
                self.decoder_latency_overstep_count += 1;

                if self.decoder_latency_overstep_count == config.latency_overstep_frames {
                    self.dynamic_decoder_max_bytes_per_frame = f32::min(
                        self.packet_bytes_average.get_average(),
                        self.dynamic_decoder_max_bytes_per_frame,
                    ) * config
                        .latency_overstep_multiplier;

                    self.update_needed = true;

                    self.decoder_latency_overstep_count = 0;
                }
            } else {
                self.decoder_latency_overstep_count = 0;
            }
        }
    }

    pub fn get_encoder_params(
        &mut self,
        config: &BitrateConfig,
    ) -> Option<(DynamicEncoderParams, BitrateDirectives)> {
        let now = Instant::now();

        let config_changed = self.previous_config.as_ref() != Some(config);
        if config_changed {
            self.previous_config = Some(config.clone());
            // Continue: always update the bitrate when the settings changed.
        } else if self.client_queue_delay_us.is_none()
            && !self.update_needed
            && (now < self.last_update_instant + UPDATE_INTERVAL
                || matches!(config.mode, BitrateMode::ConstantMbps(_)))
        {
            // **This early return is why the queue-delay cap never reached the encoder.**
            //
            // `None` here is what the FFI turns into `updated: 0`, and `updated` is the only thing
            // the C++ side looks at (`VideoEncoderNVENC::Transmit` reconfigures the encoder only
            // when it is set) — so in a constant-rate session the encoder was reconfigured once, at
            // init, and never again. That is precisely the session where nothing else will lower the
            // bitrate, and it is the second time this path has hidden the cap: the first was the
            // limiter's position inside the adaptive arm.
            //
            // A queue-delay report now bypasses it. The materiality check below is what keeps that
            // from meaning a reconfigure every frame.
            return None;
        }

        self.last_update_instant = now;
        self.update_needed = false;

        let frame_interval = if config.adapt_to_framerate.enabled() {
            self.frame_interval_average.get_average()
        } else {
            self.nominal_frame_interval
        };

        let mut bitrate_directives = BitrateDirectives::default();

        // What the settings asked for, before any limiter: the reference point the quality rung's
        // floor is a fraction of, so a client that is starving cannot be talked down to nothing.
        let nominal_bitrate_bps = match &config.mode {
            BitrateMode::ConstantMbps(bitrate_mbps) => *bitrate_mbps as f32 * 1e6,
            BitrateMode::Adaptive {
                max_throughput_mbps, ..
            } => match max_throughput_mbps {
                Switch::Enabled(mbps) => *mbps as f32 * 1e6,
                Switch::Disabled => 0.0,
            },
        };

        let mut bitrate_bps = match &config.mode {
            BitrateMode::ConstantMbps(bitrate_mbps) => *bitrate_mbps as f32 * 1e6,
            BitrateMode::Adaptive {
                saturation_multiplier,
                max_throughput_mbps,
                min_throughput_mbps,
                max_network_latency_ms,
                encoder_latency_limiter,
                decoder_latency_limiter,
            } => {
                let packet_bytes_average = self.packet_bytes_average.get_average();
                let network_latency_average_s =
                    self.network_latency_average.get_average().as_secs_f32();

                let mut throughput_bps =
                    packet_bytes_average * 8.0 * saturation_multiplier / network_latency_average_s;
                bitrate_directives.scaled_calculated_throughput_bps = Some(throughput_bps);

                if decoder_latency_limiter.enabled() {
                    throughput_bps =
                        f32::min(throughput_bps, self.dynamic_decoder_max_bytes_per_frame);
                    bitrate_directives.decoder_latency_limiter_bps =
                        Some(self.dynamic_decoder_max_bytes_per_frame);
                }

                if let Switch::Enabled(max_ms) = max_network_latency_ms {
                    let max_bps =
                        throughput_bps * (*max_ms as f32 / 1000.0) / network_latency_average_s;
                    throughput_bps = f32::min(throughput_bps, max_bps);

                    bitrate_directives.network_latency_limiter_bps = Some(max_bps);
                }

                if let Switch::Enabled(config) = encoder_latency_limiter {
                    // Note: this assumes linear relationship between bitrate and encoder latency
                    // but this may not be the case
                    let saturation = self.encoder_latency_average.get_average().as_secs_f32()
                        / self.nominal_frame_interval.as_secs_f32();
                    let max_bps = throughput_bps * config.max_saturation_multiplier / saturation;
                    bitrate_directives.encoder_latency_limiter_bps = Some(max_bps);

                    if saturation > config.max_saturation_multiplier {
                        throughput_bps = f32::min(throughput_bps, max_bps);
                    }
                }

                if let Switch::Enabled(max) = max_throughput_mbps {
                    let max_bps = *max as f32 * 1e6;
                    throughput_bps = f32::min(throughput_bps, max_bps);

                    bitrate_directives.manual_max_throughput_bps = Some(max_bps);
                }
                if let Switch::Enabled(min) = min_throughput_mbps {
                    let min_bps = *min as f32 * 1e6;
                    throughput_bps = f32::max(throughput_bps, min_bps);

                    bitrate_directives.manual_min_throughput_bps = Some(min_bps);
                }

                // NB: Here we assign the calculated throughput to the requested bitrate. This is
                // crucial for the working of the adaptive bitrate algorithm. The goal is to
                // optimally occupy the available bandwidth, which is when the bitrate corresponds
                // to the throughput.
                throughput_bps
            }
        };

        // **The receiver's own measurement — and it applies to every bitrate mode.**
        //
        // It was inside the adaptive arm first, which is where it was written and where it was
        // invisible: the session's mode is `ConstantMbps`, so the limiter never ran and the client's
        // report of 14–18 ms of queueing delay changed nothing. But a fixed rate is exactly the case
        // this exists for — the user has pinned the bitrate, nothing else in the pipeline will ever
        // lower it, and the client is saying its frames are arriving after they were needed.
        //
        // Every other limiter here is the server's view of the link, or a latency the server measured
        // on its own side. This one is the receiver saying it is behind, and a shard still queued and
        // a shard lost are the same fact from there — which is why nothing else the client can see
        // was usable.
        //
        // ## What is given up, and in what order
        //
        // **Latency first, then frame rate, then quality — and integrity never.**
        //
        // 1. *Latency.* A client that is a little behind is not in trouble: it is showing the frames
        //    it has, a few milliseconds late, and that is what the hold is for. So the first thing
        //    this does about a queueing report is **nothing**, up to a whole frame interval of it.
        //    (A third of a frame was the first target, and it fired on links that were working.)
        // 2. *Frame rate.* Past that, the client is not going to catch up while being fed at this
        //    rate, so send fewer frames. That costs smoothness, and a stream at half the frame rate
        //    looks fine — which is the whole reason it comes before quality.
        // 3. *Quality.* Only when the frame rate has been halved twice does the bitrate come down.
        //
        // Integrity is not on the list, and it has a floor of its own in
        // `x_transport::AdaptiveConfig::min`: the parity ratio is never relaxed to zero, whatever the
        // ladder is doing. A hole is the one outcome that is not allowed to be a trade.
        let frame_divisor = self.ladder_frame_divisor();
        bitrate_directives.degrade_frame_divisor = Some(frame_divisor);
        bitrate_directives.degrade_starving = Some(self.is_degrading());
        if let Some(queue_us) = self.client_queue_delay_us {
            let latency_budget_us = self.nominal_frame_interval.as_micros() as f32;
            // Quality is the third rung: it is the bitrate that comes down, and only once the frame
            // rate has already been reduced to its floor.
            let by_deficit = self.client_missing_permille.map(|missing| {
                bitrate_bps * (1.0 - missing.min(1000) as f32 / 1000.0).max(0.05)
            });
            // The same number, carried forward as state: the deficit says how much too much is being
            // sent, and the answer has to survive between reports or the rate climbs straight back to
            // the setting the moment a report reads clean.
            if let Some(deficit) = by_deficit {
                let carried = self.degrade_bitrate_bps.unwrap_or(f32::MAX);
                self.degrade_bitrate_bps = Some(carried.min(deficit).max(QUALITY_FLOOR_BPS));
            }
            let by_deficit = self.degrade_bitrate_bps;
            let by_queue = (queue_us as f32 > latency_budget_us)
                .then(|| bitrate_bps * (latency_budget_us / 3.0) / queue_us as f32);
            if let Some(max_bps) = match (by_queue, by_deficit) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            } {
                // A quality floor as well: below this the picture is not worth sending at all, and
                // the honest lever left is the frame rate, which is already at its limit — so the
                // rate stops here and the log says the ladder is exhausted.
                let floor_bps = QUALITY_FLOOR_BPS.min(nominal_bitrate_bps.max(QUALITY_FLOOR_BPS));
                let capped = f32::max(f32::min(bitrate_bps, max_bps), floor_bps.min(bitrate_bps));
                bitrate_bps = capped;
                bitrate_directives.client_queue_limiter_bps = Some(max_bps);
            }
        }

        // ---------------------------------------------------------------------------------------
        // **The budget, and the one number everything is solved from.**
        //
        // The client reports the fastest it has read datagrams; the budget is seventy percent of that
        // (the measurement is a lower bound, so planning at it would plan at the offered load). From
        // the budget: the frame rate that keeps a frame above the size at which it is a picture, and
        // then the bytes that frame may be. The encoder is asked for a rate whose *per-frame* share is
        // that many bytes — the C++ side derives its VBV from `bitrate / framerate`, so the frame size
        // is controlled exactly by what is asked for here.
        //
        // This replaces the proportional bitrate cap. That cap could not bind: it was derived from a
        // queueing delay, and a client that completes nothing never produces one. A datagram count
        // measured off the socket is true even when every frame in the window was thrown away.
        //
        // In the bootstrap the same arithmetic is done against one small frame instead: the frame the
        // whole stream is waiting for must be readable, not good.
        // ---------------------------------------------------------------------------------------
        let nominal_fps = 1.0 / frame_interval.as_secs_f32().max(1e-6);
        if self.bootstrap {
            let target_bytes = self.bootstrap_target_bytes as f32;
            bitrate_bps = target_bytes * 8.0 * nominal_fps;
            bitrate_directives.bootstrap_frame_bytes = Some(self.bootstrap_target_bytes);
        } else if let Some(budget) = self.delivery_budget_per_sec {
            let target_fps = self.frame_rate_for_budget(nominal_fps);
            let bytes = self
                .frame_bytes_budget(target_fps)
                .unwrap_or(MIN_FRAME_BYTES as usize) as f32;
            bitrate_bps = bytes * 8.0 * nominal_fps;
            bitrate_directives.delivery_budget_per_sec = Some(budget as f32);
            bitrate_directives.frame_bytes_budget = Some(bytes as usize);
            bitrate_directives.target_frames_per_sec = Some(target_fps);
        }

        // A reconfigure is a **full encoder re-initialisation** on the C++ side, so a value the
        // encoder already has is not worth sending — and now that the cap can make this function run
        // every frame in a constant-rate session, that matters. The adaptive path is left exactly as
        // it was: its own update cadence is what decides there.
        if matches!(config.mode, BitrateMode::ConstantMbps(_)) && !config_changed {
            let material = self
                .last_returned_bitrate_bps
                .is_none_or(|last| last <= 0.0 || (last - bitrate_bps).abs() > last * 0.02);
            if !material {
                return None;
            }
        }
        self.last_returned_bitrate_bps = Some(bitrate_bps);

        bitrate_directives.requested_bitrate_bps = bitrate_bps;

        Some((
            DynamicEncoderParams {
                bitrate_bps,
                // **The frame rate the encoder will actually run at**, not the rate the wire will
                // carry. The two differ once the ladder is dropping frames, and telling the encoder
                // the wire rate is not a harmless approximation: the C++ side derives its VBV and its
                // per-frame budget as `bitrate / framerate`, so a frame rate divided by twelve gave
                // every frame twelve times the bits — 39 538 bytes per frame from an encoder asked for
                // 2.07 Mbps, exactly its VBV, and a stream that fitted the queue no better than
                // before. The divisor is a wire decision and belongs on the wire.
                framerate: 1.0 / f32::min(frame_interval.as_secs_f32(), 1.0),
            },
            bitrate_directives,
        ))
    }

    /// How many bytes a frame may be, given the budget and the frame rate that is being asked for.
    ///
    /// Parity counts *inside* the budget: a frame of N datagrams includes its repair shards, because
    /// the repair shards are datagrams the client has to read, and a budget that forgot them would be
    /// overrun by exactly the ratio the FEC is configured for.
    pub fn frame_bytes_budget(&self, frames_per_sec: f32) -> Option<usize> {
        let budget = self.delivery_budget_per_sec?;
        if frames_per_sec <= 0.0 {
            return None;
        }
        // A frame's share of the budget, converted to bytes with the shard size and the overhead the
        // parity policy charges.
        let datagrams_per_frame = budget / frames_per_sec as f64;
        let payload_datagrams = datagrams_per_frame / (1.0 + self.parity_ratio as f64);
        Some((payload_datagrams * self.shard_bytes as f64) as usize)
    }

    /// The frame rate to send at, given the budget: the nominal rate while each frame can still hold
    /// enough bytes to be worth showing, then halved, and so on down to the floor.
    ///
    /// **Frame rate is the actuator, not the bitrate**, because it is the only one that keeps a frame
    /// above the size at which it can be read at all. At a 190 datagram/s budget and 72 frames/s a
    /// frame gets two and a half datagrams; at 15 frames/s it gets eleven, which is a picture. The
    /// order the ladder owes — latency, then frame rate, then quality — is the same order this takes.
    pub fn frame_rate_for_budget(&self, nominal_fps: f32) -> f32 {
        let Some(_) = self.delivery_budget_per_sec else {
            return nominal_fps;
        };
        let mut fps = nominal_fps;
        while fps > 15.0 {
            match self.frame_bytes_budget(fps) {
                Some(bytes) if bytes as f64 >= MIN_FRAME_BYTES => break,
                _ => fps /= 2.0,
            }
        }
        fps.max(15.0)
    }

    /// The second rung of the degradation ladder: how many frames to skip for every one sent.
    ///
    /// A function of how far behind the client says it is, in units of the frame interval — the only
    /// unit that means anything to a client whose job is to show one frame per interval. The caller
    /// applies it to the media sender, which is where a frame can actually be dropped: the encoder is
    /// in another process, and skipping *there* would encode bits never sent.
    pub fn ladder_frame_divisor(&self) -> u32 {
        // **From the budget.** The frame rate is the actuator: the budget says how many datagrams the
        // client can read, and a frame needs a minimum number of them to be a picture, so the rate is
        // whatever fits. Everything else the ladder used to do here — a queueing delay, an
        // inferred deficit — was a proxy for a number the client can now simply report.
        if self.delivery_budget_per_sec.is_some() {
            let nominal_fps = 1.0 / self.nominal_frame_interval.as_secs_f32().max(1e-6);
            let target_fps = self.frame_rate_for_budget(nominal_fps);
            if target_fps > 0.0 {
                return ((nominal_fps / target_fps).round() as u32).clamp(1, DEGRADE_MAX_FRAME_DIVISOR);
            }
        }

        // Two independent reasons to send fewer frames, and the larger answer wins.
        //
        // 1. **The client is behind**: it is reading slowly enough that the queue is backing up.
        //    One step per frame interval of queueing, from "nothing" to the cap: the rung is a dial
        //    rather than a switch, so a client two frames behind is not treated like one six behind.
        let by_queue = match self.client_queue_delay_us {
            Some(queue_us) => {
                let interval_us = self.nominal_frame_interval.as_micros() as f32;
                if interval_us <= 0.0 {
                    1
                } else {
                    let behind = queue_us as f32 / interval_us;
                    if behind < LADDER_FRAMERATE_FROM {
                        1
                    } else {
                        (behind.floor() as u32).min(DEGRADE_MAX_FRAME_DIVISOR)
                    }
                }
            }
            None => 1,
        };

        // 2. **The client is missing shards.** A client that is not behind but is not receiving is a
        //    client being sent more than it can take, and the queue delay cannot see it: the frames
        //    it completes are the small ones, and it completes them promptly. This rung is a
        //    controller with state — see `report_client_missing` — because the answer to "a third of
        //    every frame is missing" is a *rate*, not a multiple of the last sample.
        by_queue.max(self.degrade_divisor).min(DEGRADE_MAX_FRAME_DIVISOR)
    }
}
