//! Adapters: each one reduces one source format to [`MetricSamples`]. Parsing is by header name
//! for our own CSVs (robust to column additions) and by exact log-line shapes for vrlink
//! (whose format is fixed in its binary).

use crate::schema::{Metric, MetricSamples};
use std::collections::BTreeMap;

fn push(map: &mut BTreeMap<Metric, Vec<f64>>, metric: Metric, value: f64) {
    if value.is_finite() {
        map.entry(metric).or_default().push(value);
    }
}

/// `GEMPLINK_DEBUG_CSV` from the client. Column names come from
/// `alvr_client_core::media_plane::PlaneStats::csv_header` and are matched by name, so new
/// columns do not break the adapter.
pub fn gemlink_client_csv(contents: &str) -> Vec<MetricSamples> {
    let mut map: BTreeMap<Metric, Vec<f64>> = BTreeMap::new();
    let mut lines = contents.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return vec![];
    };
    let columns: Vec<&str> = header.split(',').map(str::trim).collect();
    let column = |name: &str| columns.iter().position(|c| *c == name);

    let (Some(m2p), Some(d50), Some(d95)) = (
        column("m2p_avg_ms"),
        column("decode_p50_ms"),
        column("decode_p95_ms"),
    ) else {
        // Not a GemLink client CSV (wrong header) — nothing to do.
        return vec![];
    };

    for line in lines {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let value = |index: usize| fields.get(index).and_then(|v| v.parse::<f64>().ok());
        if let Some(v) = value(m2p) {
            push(&mut map, Metric::M2pSelfReportedMs, v);
        }
        if let Some(v) = value(d50) {
            push(&mut map, Metric::DecodeP50Ms, v);
        }
        if let Some(v) = value(d95) {
            push(&mut map, Metric::DecodeP95Ms, v);
        }
    }

    to_samples(map)
}

/// `GEMPLINK_TRACE_CSV` from the server: per-frame server-side pipeline latency
/// (`present → encoded`, µs, PC clock). GemLink-internal context, not an M2P number and never
/// compared across software — the schema carries it so the trace parses at all.
pub fn gemlink_server_trace(contents: &str) -> Vec<MetricSamples> {
    let mut map: BTreeMap<Metric, Vec<f64>> = BTreeMap::new();
    let mut lines = contents.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return vec![];
    };
    let columns: Vec<&str> = header.split(',').map(str::trim).collect();
    let column = |name: &str| columns.iter().position(|c| *c == name);

    let Some(pipeline) = column("total_pipeline_latency_us") else {
        return vec![];
    };

    for line in lines {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if let Some(v) = fields.get(pipeline).and_then(|v| v.parse::<f64>().ok()) {
            push(&mut map, Metric::ServerPipelineUs, v);
        }
    }

    to_samples(map)
}

/// vrlink headset stdout. The lines are exact-format (from the binary's strings):
/// `%6.3f mbps %3d FPS`, `PreH %4.1fms`, `PreC %4.1fms`, `DeadL %4.1fms`.
pub fn vrlink_client_log(contents: &str) -> Vec<MetricSamples> {
    let mut map: BTreeMap<Metric, Vec<f64>> = BTreeMap::new();

    for line in contents.lines() {
        let line = line.trim();
        if let Some((mbps, fps)) = parse_mbps_fps(line) {
            push(&mut map, Metric::ThroughputMbps, mbps);
            push(&mut map, Metric::Fps, fps);
            continue;
        }
        for (token, metric) in [
            ("PreH", Metric::VrlinkPreHMs),
            ("PreC", Metric::VrlinkPreCMs),
            ("DeadL", Metric::VrlinkDeadLMs),
        ] {
            if let Some(rest) = line.strip_prefix(token) {
                let rest = rest.trim();
                if let Some(value) = rest
                    .strip_suffix("ms")
                    .and_then(|v| v.trim().parse::<f64>().ok())
                {
                    push(&mut map, metric, value);
                }
            }
        }
        if line.contains("CAudioJitterBuffer underflow") {
            push(&mut map, Metric::AudioUnderruns, 1.0);
        }
        if line.contains("FEC decode fail") {
            push(&mut map, Metric::FecFailures, 1.0);
        }
    }

    to_samples(map)
}

/// `%6.3f mbps %3d FPS` — mbps and FPS in one line. Also matches the same shape wherever the
/// PC driver prints it.
pub fn parse_mbps_fps(line: &str) -> Option<(f64, f64)> {
    let mbps_at = line.find("mbps")?;
    let before = &line[..mbps_at];
    let after = &line[mbps_at + "mbps".len()..];
    let mbps = before.split_whitespace().last()?.parse::<f64>().ok()?;
    let fps = after
        .split_whitespace()
        .find_map(|token| token.parse::<f64>().ok())?;
    Some((mbps, fps))
}

fn to_samples(map: BTreeMap<Metric, Vec<f64>>) -> Vec<MetricSamples> {
    map.into_iter()
        .map(|(metric, samples)| MetricSamples { metric, samples })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vrlink_log_yields_throughput_stages_and_underruns() {
        let log = "\
16:00:17.400 some prefix 12.345 mbps  72 FPS
PreH 3.2ms
PreC 1.1ms
DeadL 8.9ms
CAudioJitterBuffer underflow 12 3
FEC decode fail 1 2 3
";
        let samples = vrlink_client_log(log);
        let find = |m: Metric| {
            samples
                .iter()
                .find(|s| s.metric == m)
                .map(|s| s.samples.clone())
                .unwrap_or_default()
        };
        assert_eq!(find(Metric::ThroughputMbps), vec![12.345]);
        assert_eq!(find(Metric::Fps), vec![72.0]);
        assert_eq!(find(Metric::VrlinkPreHMs), vec![3.2]);
        assert_eq!(find(Metric::VrlinkPreCMs), vec![1.1]);
        assert_eq!(find(Metric::VrlinkDeadLMs), vec![8.9]);
        assert_eq!(find(Metric::AudioUnderruns).len(), 1);
        assert_eq!(find(Metric::FecFailures).len(), 1);
    }

    #[test]
    fn gemlink_csv_is_parsed_by_header_name() {
        let header = "datagrams_in,dropped_by_source,rejected,frames_presented,across_hole,\
frames_held,held_no_keyframe,held_gap,held_datagram_loss,held_unconfirmed_reference,\
held_decoder,abandoned,repaired_fec,keyframes_in,keyframes_clean,nacks,nacks_suppressed,\
keyframe_requests,resets,acks,read_per_sec,m2p_avg_ms,decode_p50_ms,decode_p95_ms,\
decode_budget_ok";
        let csv =
            format!("{header}\n100,0,0,90,5,0,0,0,0,0,0,0,0,1,1,2,0,1,0,89,80,45.20,4.10,6.80,1\n");
        let samples = gemlink_client_csv(&csv);
        let find = |m: Metric| {
            samples
                .iter()
                .find(|s| s.metric == m)
                .map(|s| s.samples.clone())
                .unwrap_or_default()
        };
        assert_eq!(find(Metric::M2pSelfReportedMs), vec![45.20]);
        assert_eq!(find(Metric::DecodeP50Ms), vec![4.10]);
        assert_eq!(find(Metric::DecodeP95Ms), vec![6.80]);
    }

    #[test]
    fn distributions_percentile_correctly() {
        use crate::schema::MetricDistribution;
        let dist = MetricDistribution::from_samples("ms", vec![1.0, 2.0, 3.0, 4.0, 100.0]);
        assert_eq!(dist.p50, Some(3.0));
        assert_eq!(dist.p95, Some(100.0));
        assert_eq!(dist.min, Some(1.0));
        assert_eq!(dist.n, 5);
    }
}
