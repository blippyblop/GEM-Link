// Opus codec for the audio stream, both directions (user directive 2026-10-06: Steam Link-style
// low-latency Opus). vrlink ships raw PCM with link-level FEC; Steam Remote Play ships Opus; this
// is Opus with in-band FEC for the microphone, which is the "Audio FEC" property done in-codec.
// Raw PCM remains available as the negotiated fallback and is what a Linux dev server uses.
//
// libopus is compiled from source by opusic-sys (cmake), so the same code cross-builds for
// aarch64 (the Frame) and builds for the Windows server on the box.

use alvr_common::anyhow::{Result, anyhow};
use opusic_sys as ffi;

/// Sample rates libopus accepts. Anything else must be resampled before encoding.
pub const OPUS_RATES_HZ: [u32; 5] = [8000, 12000, 16000, 24000, 48000];

/// Full-band is the norm (48 kHz); 44.1 kHz devices are mapped up to 48 kHz. A zero or unknown
/// rate is treated as full-band: callers guard "no device" separately, and this function's only
/// job is to name a rate libopus accepts.
pub fn nearest_opus_rate(rate: u32) -> u32 {
    if rate != 0 && OPUS_RATES_HZ.contains(&rate) {
        rate
    } else if rate != 0 && rate < 10_000 {
        8000
    } else if rate != 0 && rate < 14_000 {
        12_000
    } else if rate != 0 && rate < 20_000 {
        16_000
    } else if rate != 0 && rate < 36_000 {
        24_000
    } else {
        48_000
    }
}

/// Linear-interpolation resampler for device rates opus does not accept (44.1 kHz -> 48 kHz).
/// Music on a 44.1 kHz device loses a little high-frequency accuracy; that is the honest cost of
/// keeping the codec path free of a full resampler dependency.
pub fn resample_linear(input: &[i16], channels: usize, from: u32, to: u32, output: &mut Vec<i16>) {
    if from == to || channels == 0 || input.is_empty() {
        output.extend_from_slice(input);
        return;
    }

    let frames = input.len() / channels;
    if frames == 0 {
        return;
    }

    let out_frames = (frames as u64 * to as u64 / from as u64) as usize;
    output.reserve(out_frames * channels);

    for out_frame in 0..out_frames {
        // Position in the input, in frames.
        let pos = out_frame as f64 * (frames - 1) as f64 / out_frames.max(1) as f64;
        let index = pos.floor() as usize;
        let frac = (pos - index as f64) as f32;
        let next = (index + 1).min(frames - 1);

        for channel in 0..channels {
            let a = input[index * channels + channel] as f32;
            let b = input[next * channels + channel] as f32;
            let sample = a + (b - a) * frac;
            output.push(sample.clamp(-32768.0, 32767.0) as i16);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Application {
    /// Speech; enables the SILK path, in-band FEC and DTX. Microphone.
    Voip,
    /// Music and general content. Game audio when the restricted-lowdelay mode is too brittle.
    Audio,
    /// Lowest algorithmic delay; no resampling, no SILK. Game audio.
    LowDelay,
}

impl Application {
    fn to_ffi(self) -> std::ffi::c_int {
        match self {
            Application::Voip => ffi::OPUS_APPLICATION_VOIP,
            Application::Audio => ffi::OPUS_APPLICATION_AUDIO,
            Application::LowDelay => ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }
    }
}

pub struct OpusEncoder {
    encoder: *mut ffi::OpusEncoder,
    channels: usize,
    /// Samples per channel per encoded frame (e.g. 480 at 48 kHz / 10 ms).
    frame_samples: usize,
    scratch: Vec<u8>,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_rate: u32,
        channels: usize,
        application: Application,
        frame_ms: u32,
        bitrate_bps: u32,
        inband_fec: bool,
        dtx: bool,
        expected_loss_percent: i32,
    ) -> Result<Self> {
        let mut error = 0;
        let encoder = unsafe {
            ffi::opus_encoder_create(
                sample_rate as std::ffi::c_int,
                channels as std::ffi::c_int,
                application.to_ffi(),
                &mut error,
            )
        };
        if error != ffi::OPUS_OK || encoder.is_null() {
            return Err(anyhow!(
                "opus_encoder_create failed: rate {sample_rate} ch {channels}: error {error}"
            ));
        }

        let frame_samples = sample_rate as usize * frame_ms as usize / 1000;

        macro_rules! set {
            ($request:expr, $value:expr, $name:literal) => {
                let code = unsafe { ffi::opus_encoder_ctl(encoder, $request, $value) };
                if code != ffi::OPUS_OK {
                    unsafe { ffi::opus_encoder_destroy(encoder) };
                    return Err(anyhow!(concat!("opus ctl ", $name, " failed: {}"), code));
                }
            };
        }

        set!(ffi::OPUS_SET_BITRATE_REQUEST, bitrate_bps as i32, "bitrate");
        if let Application::Voip = application {
            set!(
                ffi::OPUS_SET_INBAND_FEC_REQUEST,
                inband_fec as i32,
                "inband FEC"
            );
            set!(
                ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
                expected_loss_percent,
                "loss estimate"
            );
            set!(ffi::OPUS_SET_DTX_REQUEST, dtx as i32, "DTX");
        }

        Ok(Self {
            encoder,
            channels,
            frame_samples,
            scratch: vec![0; 4000],
        })
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Encode exactly one frame (`frame_samples * channels` samples). Returns the packet length.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8]> {
        debug_assert_eq!(pcm.len(), self.frame_samples * self.channels);
        let len = unsafe {
            ffi::opus_encode(
                self.encoder,
                pcm.as_ptr(),
                self.frame_samples as std::ffi::c_int,
                self.scratch.as_mut_ptr(),
                self.scratch.len() as std::ffi::c_int,
            )
        };
        if len < 0 {
            return Err(anyhow!("opus_encode failed: {len}"));
        }
        Ok(&self.scratch[..len as usize])
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.encoder) };
    }
}

pub struct OpusDecoder {
    decoder: *mut ffi::OpusDecoder,
    channels: usize,
    /// Frame size the decoder was configured to expect, used for loss concealment.
    frame_samples: usize,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: usize, frame_ms: u32) -> Result<Self> {
        let mut error = 0;
        let decoder = unsafe {
            ffi::opus_decoder_create(
                sample_rate as std::ffi::c_int,
                channels as std::ffi::c_int,
                &mut error,
            )
        };
        if error != ffi::OPUS_OK || decoder.is_null() {
            return Err(anyhow!(
                "opus_decoder_create failed: rate {sample_rate} ch {channels}: error {error}"
            ));
        }

        Ok(Self {
            decoder,
            channels,
            frame_samples: sample_rate as usize * frame_ms as usize / 1000,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode one packet, appending samples to `output`. A `None` packet produces loss
    /// concealment (comfort continuation) for the expected frame duration.
    pub fn decode(&mut self, packet: Option<&[u8]>, output: &mut Vec<i16>) -> Result<()> {
        let start = output.len();
        output.resize(start + self.frame_samples * self.channels, 0);

        let (data, len) = match packet {
            Some(packet) => (packet.as_ptr(), packet.len() as std::ffi::c_int),
            None => (std::ptr::null(), 0),
        };

        let samples = unsafe {
            ffi::opus_decode(
                self.decoder,
                data,
                len,
                output[start..].as_mut_ptr(),
                self.frame_samples as std::ffi::c_int,
                0,
            )
        };
        if samples < 0 {
            // Drop the reserved region rather than emit garbage.
            output.truncate(start);
            return Err(anyhow!("opus_decode failed: {samples}"));
        }

        let produced = samples as usize * self.channels;
        output.truncate(start + produced);
        if packet.is_some() {
            self.frame_samples = samples as usize;
        }
        Ok(())
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.decoder) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_sweep(frame_samples: usize, channels: usize, offset_frames: usize) -> Vec<i16> {
        (0..frame_samples * channels)
            .map(|i| {
                let t = (i / channels + offset_frames * frame_samples) as f32;
                (t * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 12_000.0
            })
            .map(|s| s as i16)
            .collect()
    }

    #[test]
    fn opus_roundtrip_decodes_the_frames_it_encoded() {
        let mut encoder = OpusEncoder::new(
            48_000,
            2,
            Application::LowDelay,
            10,
            96_000,
            false,
            false,
            0,
        )
        .expect("encoder");
        let mut decoder = OpusDecoder::new(48_000, 2, 10).expect("decoder");

        let mut total_samples = 0;
        for frame in 0..5 {
            let packet = encoder
                .encode(&sine_sweep(encoder.frame_samples(), 2, frame))
                .expect("encode")
                .to_vec();
            assert!(!packet.is_empty());

            let mut pcm = Vec::new();
            decoder.decode(Some(&packet), &mut pcm).expect("decode");
            assert_eq!(pcm.len(), 480 * 2, "10 ms stereo at 48 kHz");
            total_samples += pcm.len();
        }
        assert_eq!(total_samples, 5 * 480 * 2);
    }

    #[test]
    fn opus_concealment_fills_a_dropped_packet_without_failing() {
        let mut encoder =
            OpusEncoder::new(48_000, 1, Application::Voip, 20, 24_000, true, true, 10)
                .expect("encoder");
        let mut decoder = OpusDecoder::new(48_000, 1, 20).expect("decoder");

        let frames: Vec<Vec<u8>> = (0..3)
            .map(|frame| {
                encoder
                    .encode(&sine_sweep(encoder.frame_samples(), 1, frame))
                    .expect("encode")
                    .to_vec()
            })
            .collect();

        let mut pcm = Vec::new();
        decoder.decode(Some(&frames[0]), &mut pcm).expect("frame 0");

        // Frame 1 is lost on the wire: the receiver asks for concealment.
        decoder.decode(None, &mut pcm).expect("PLC");
        assert_eq!(pcm.len(), 2 * 960, "20 ms mono at 48 kHz, twice");

        // Frame 2 still decodes: opus packets are self-contained frames.
        decoder.decode(Some(&frames[2]), &mut pcm).expect("frame 2");
        assert_eq!(pcm.len(), 3 * 960);
        assert!(pcm.iter().any(|s| *s != 0));
    }

    #[test]
    fn nearest_opus_rate_maps_the_common_device_rates() {
        assert_eq!(nearest_opus_rate(48_000), 48_000);
        assert_eq!(nearest_opus_rate(44_100), 48_000);
        assert_eq!(nearest_opus_rate(16_000), 16_000);
        assert_eq!(nearest_opus_rate(22_050), 24_000);
        assert_eq!(nearest_opus_rate(0), 48_000);
    }

    #[test]
    fn resampler_scales_the_frame_count_and_keeps_energy() {
        let input: Vec<i16> = (0..441)
            .map(|i| ((i as f32 * 0.05).sin() * 10_000.0) as i16)
            .collect();
        let mut output = Vec::new();
        resample_linear(&input, 1, 44_100, 48_000, &mut output);

        let expected = 441_u64 * 48_000 / 44_100;
        assert_eq!(output.len() as u64, expected);
        let input_peak = input.iter().map(|s| s.abs()).max().unwrap();
        let output_peak = output.iter().map(|s| s.abs()).max().unwrap();
        assert!(output_peak > input_peak / 2, "energy roughly preserved");
    }

    #[test]
    fn dtx_produces_packets_the_decoder_accepts() {
        // Silence with DTX on must produce tiny (discontinuity) packets that still decode.
        let mut encoder =
            OpusEncoder::new(48_000, 1, Application::Voip, 20, 24_000, false, true, 5)
                .expect("encoder");
        let mut decoder = OpusDecoder::new(48_000, 1, 20).expect("decoder");

        let silence = vec![0i16; 960];
        for _ in 0..10 {
            let packet = encoder.encode(&silence).expect("encode").to_vec();
            let mut pcm = Vec::new();
            decoder.decode(Some(&packet), &mut pcm).expect("decode");
            assert_eq!(pcm.len(), 960);
        }
    }
}
