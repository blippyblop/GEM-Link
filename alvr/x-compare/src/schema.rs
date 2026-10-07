//! The normalized metric set every adapter reduces a stack's self-measurements to.
//!
//! The one rule this module exists to enforce: **a number travels with its semantics**. GemLink
//! measures motion-to-photon on the device clock; vrlink prints `PreH/PreC/DeadL` stage
//! estimates whose semantics are Valve-internal. Both are "latency numbers" and comparing them
//! directly is how a benchmark starts lying. Every metric therefore declares
//! [`Metric::comparable_across_software`], and the report honors it: only comparable metrics
//! are shown as a scoreboard; everything else is per-software context.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub software: String,
    pub sources: Vec<String>,
    pub metrics: BTreeMap<String, MetricDistribution>,
}

/// A metric identity: what it is, and what it may be compared against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Metric {
    Fps,
    ThroughputMbps,
    /// Motion-to-photon as the software itself measures it, device-clock
    /// (input sample → submit → predicted vsync). GemLink-internal today; comparable only
    /// against a future stack that measures the same way.
    M2pSelfReportedMs,
    /// Decode stage latency, device-clock (packet arrival → decoded).
    DecodeP50Ms,
    DecodeP95Ms,
    /// Latency stage estimates with software-internal semantics (vrlink's
    /// `PreH`/`PreC`/`DeadL`). Never cross-software comparable; reported per software.
    VrlinkPreHMs,
    VrlinkPreCMs,
    VrlinkDeadLMs,
    /// Frames the FEC could not repair.
    FecFailures,
    /// Packets lost since the last reset, as the stack reports it.
    PacketsLost,
    /// Audio jitter-buffer underruns.
    AudioUnderruns,
    /// The link-control state: read ceiling and delivery budget (GemLink mechanism).
    ReadCeilingPerSec,
    DeliveryBudgetPerSec,
    /// Server-side pipeline latency (present → encoded, µs, PC clock). GemLink-internal
    /// context from the per-frame trace; never cross-software comparable.
    ServerPipelineUs,
}

impl Metric {
    pub fn name(self) -> &'static str {
        match self {
            Metric::Fps => "fps",
            Metric::ThroughputMbps => "throughput_mbps",
            Metric::M2pSelfReportedMs => "m2p_self_reported_ms",
            Metric::DecodeP50Ms => "decode_p50_ms",
            Metric::DecodeP95Ms => "decode_p95_ms",
            Metric::VrlinkPreHMs => "vrlink_preh_ms",
            Metric::VrlinkPreCMs => "vrlink_prec_ms",
            Metric::VrlinkDeadLMs => "vrlink_deadl_ms",
            Metric::FecFailures => "fec_failures",
            Metric::PacketsLost => "packets_lost",
            Metric::AudioUnderruns => "audio_underruns",
            Metric::ReadCeilingPerSec => "read_ceiling_per_sec",
            Metric::DeliveryBudgetPerSec => "delivery_budget_per_sec",
            Metric::ServerPipelineUs => "server_pipeline_us",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Metric::Fps | Metric::AudioUnderruns => "count/s or count",
            Metric::ThroughputMbps => "mbps",
            Metric::M2pSelfReportedMs
            | Metric::DecodeP50Ms
            | Metric::DecodeP95Ms
            | Metric::VrlinkPreHMs
            | Metric::VrlinkPreCMs
            | Metric::VrlinkDeadLMs => "ms",
            Metric::FecFailures | Metric::PacketsLost => "count",
            Metric::ReadCeilingPerSec | Metric::DeliveryBudgetPerSec => "datagrams/s",
            Metric::ServerPipelineUs => "us",
        }
    }

    /// Whether two software entries may be shown as directly comparable. The stage estimates
    /// may not: their semantics are private to the software that printed them. The server
    /// pipeline trace is GemLink-internal context as well.
    pub fn comparable_across_software(self) -> bool {
        !matches!(
            self,
            Metric::VrlinkPreHMs
                | Metric::VrlinkPreCMs
                | Metric::VrlinkDeadLMs
                | Metric::ServerPipelineUs
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricDistribution {
    pub unit: &'static str,
    pub n: u64,
    pub mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
}

impl MetricDistribution {
    pub fn from_samples(unit: &'static str, mut samples: Vec<f64>) -> Self {
        samples.retain(|v| v.is_finite());
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = samples.len() as u64;
        let pick = |fraction: f64| -> Option<f64> {
            if samples.is_empty() {
                None
            } else {
                let index = ((samples.len() as f64 - 1.0) * fraction).round() as usize;
                Some(samples[index])
            }
        };
        let mean = if samples.is_empty() {
            None
        } else {
            Some(samples.iter().sum::<f64>() / samples.len() as f64)
        };
        Self {
            unit,
            n,
            mean,
            min: samples.first().copied(),
            max: samples.last().copied(),
            p50: pick(0.50),
            p95: pick(0.95),
        }
    }

    /// The percentile fields as one display row.
    pub fn row(&self) -> String {
        let f = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
        format!(
            "n={} p50={}{} p95={}{} max={}{}",
            self.n,
            f(self.p50),
            self.unit,
            f(self.p95),
            self.unit,
            f(self.max),
            self.unit,
        )
    }
}

/// What an adapter hands back: raw samples for one metric from one source file. Distributions
/// are computed once, at report time, over merged samples — never per file, so a metric spread
/// across several logs stays one distribution.
pub struct MetricSamples {
    pub metric: Metric,
    pub samples: Vec<f64>,
}
