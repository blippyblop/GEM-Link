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

/// The most the frame rate is reduced by: one frame in six.
///
/// Measured, not chosen: the emulated client reads ~300 datagrams/s, so at 72 Hz it can take about
/// four datagrams per frame, and a stream of 26-datagram frames is one it cannot read no matter what
/// the picture costs. Six is 12 fps at 72 Hz — the "drops to 15 fps for a few seconds" the ladder is
/// allowed to spend, and a great deal better than a hole.
const DEGRADE_MAX_FRAME_DIVISOR: u32 = 6;

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
        if let Some(queue_us) = self.client_queue_delay_us {
            let latency_budget_us = self.nominal_frame_interval.as_micros() as f32;
            // Quality is the third rung: it is the bitrate that comes down, and only once the frame
            // rate has already been reduced to its floor.
            if frame_divisor >= DEGRADE_MAX_FRAME_DIVISOR && queue_us as f32 > latency_budget_us {
                let target_us = latency_budget_us / 3.0;
                let max_bps = bitrate_bps * target_us / queue_us as f32;
                // A quality floor as well: below this the picture is not worth sending at all, and
                // the honest lever left is the frame rate, which is already at its limit — so the
                // rate stops here and the log says the ladder is exhausted.
                let floor_bps = QUALITY_FLOOR_BPS.min(nominal_bitrate_bps.max(QUALITY_FLOOR_BPS));
                let capped = f32::max(f32::min(bitrate_bps, max_bps), floor_bps.min(bitrate_bps));
                bitrate_bps = capped;
                bitrate_directives.client_queue_limiter_bps = Some(max_bps);
            }
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
                // The encoder is told the *effective* frame rate: at half the frames, each frame has
                // twice the bit budget, and a rate controller still modelling the nominal rate would
                // spend the stream on frames that are not being sent.
                framerate: 1.0 / f32::min(frame_interval.as_secs_f32(), 1.0) / frame_divisor as f32,
            },
            bitrate_directives,
        ))
    }

    /// The second rung of the degradation ladder: how many frames to skip for every one sent.
    ///
    /// A function of how far behind the client says it is, in units of the frame interval — the only
    /// unit that means anything to a client whose job is to show one frame per interval. The caller
    /// applies it to the media sender, which is where a frame can actually be dropped: the encoder is
    /// in another process, and skipping *there* would encode bits never sent.
    pub fn ladder_frame_divisor(&self) -> u32 {
        let Some(queue_us) = self.client_queue_delay_us else {
            return 1;
        };
        let interval_us = self.nominal_frame_interval.as_micros() as f32;
        if interval_us <= 0.0 {
            return 1;
        }
        // One step per frame interval of queueing, from "nothing" to the cap: the second rung is a
        // dial rather than a switch, so a client that is two frames behind is not treated the same as
        // one that is six behind.
        let behind = queue_us as f32 / interval_us;
        if behind < LADDER_FRAMERATE_FROM {
            return 1;
        }
        (behind.floor() as u32).min(DEGRADE_MAX_FRAME_DIVISOR)
    }
}
