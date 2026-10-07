//! The server-log adapter lives here to keep `main.rs` to the CLI. Parses the session log's
//! periodic lines: the encoder/throughput line, the decode-budget line, and the M2P line.

use crate::adapters::parse_mbps_fps;

use crate::schema::{Metric, MetricSamples};
use std::collections::BTreeMap;

pub fn gemlink_server_log(contents: &str) -> Vec<MetricSamples> {
    let mut map: BTreeMap<Metric, Vec<f64>> = BTreeMap::new();
    let mut push = |metric: Metric, value: f64| {
        if value.is_finite() {
            map.entry(metric).or_default().push(value);
        }
    };

    for line in contents.lines() {
        let line = line.trim();
        // Throughput: the encoder line carries "1.65 Mbps at 72 fps" — the same semantics as
        // vrlink's "%6.3f mbps %3d FPS", so it feeds the same comparable metrics.
        if let Some(at) = line.find(" Mbps at ") {
            let before = &line[..at];
            if let Some(mbps) = before
                .split_whitespace()
                .last()
                .and_then(|v| v.parse::<f64>().ok())
            {
                push(Metric::ThroughputMbps, mbps);
            }
            let after = &line[at + " Mbps at ".len()..];
            if let Some(fps) = after
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
            {
                push(Metric::Fps, fps);
            }
        }
        // Decode budget line.
        if let Some(rest) = line.strip_prefix("decode: p50 ") {
            if let Some(p50) = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
            {
                push(Metric::DecodeP50Ms, p50);
            }
            if let Some(p95_at) = line.find("p95 ") {
                let after = &line[p95_at + 4..];
                if let Some(p95) = after
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                {
                    push(Metric::DecodeP95Ms, p95);
                }
            }
        }
        // M2P line.
        if let Some(rest) = line.strip_prefix("motion-to-photon (input to predicted vsync): ") {
            if let Some(ms) = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
            {
                push(Metric::M2pSelfReportedMs, ms);
            }
        }
        // Link-control state.
        if let Some(at) = line.find("read ceiling ") {
            let after = &line[at + "read ceiling ".len()..];
            if let Some(ceiling) = after
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
            {
                push(Metric::ReadCeilingPerSec, ceiling);
            }
            if let Some(budget_at) = line.find("budget ") {
                let after = &line[budget_at + "budget ".len()..];
                if let Some(budget) = after
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                {
                    push(Metric::DeliveryBudgetPerSec, budget);
                }
            }
        }
    }

    let _ = parse_mbps_fps; // shared with the vrlink adapter; kept imported for symmetry
    map.into_iter()
        .map(|(metric, samples)| MetricSamples { metric, samples })
        .collect()
}
