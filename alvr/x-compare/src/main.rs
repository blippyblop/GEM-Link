//! x-compare — the software-measurable latency comparison tool.
//!
//! Pulls in how each link software benchmarks *itself* (adapters normalize each format into one
//! schema) and emits a comparison report. The rule the whole tool enforces: **a number travels
//! with its semantics** — GemLink's device-clock motion-to-photon is never shown as comparable
//! against vrlink's Valve-internal `PreH/PreC/DeadL` stage estimates, and the report says so.
//!
//! Usage:
//! ```text
//! x-compare --software GemLink --gemlink-client client.csv --gemlink-server trace.csv \
//!           --software vrlink --vrlink-log vrlink.log \
//!           [--out report.md]
//! ```
//! Each `--software NAME` starts a session; the source flags after it attach files to that
//! session. Multiple `--software` sections compare side by side.

mod adapters;
mod adapters_server_log;
mod schema;

use schema::{Metric, MetricDistribution, MetricSamples, Session};
use std::collections::BTreeMap;

struct Args {
    sessions: Vec<(String, Vec<SourceFile>)>,
    out: Option<String>,
}

struct SourceFile {
    kind: SourceKind,
    path: String,
}

#[derive(Clone, Copy, PartialEq)]
enum SourceKind {
    GemlinkClientCsv,
    GemlinkServerTrace,
    GemlinkServerLog,
    VrlinkClientLog,
    VrlinkServerTrace,
}

fn parse_args() -> Args {
    let mut sessions: Vec<(String, Vec<SourceFile>)> = Vec::new();
    let mut out = None;
    let mut args = std::env::args().skip(1).peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--software" => {
                let name = args.next().unwrap_or_else(|| {
                    eprintln!("--software requires a name");
                    std::process::exit(2);
                });
                sessions.push((name, Vec::new()));
            }
            "--gemlink-client" => attach(&mut sessions, SourceKind::GemlinkClientCsv, &mut args),
            "--gemlink-server" | "--gemlink-trace" => {
                attach(&mut sessions, SourceKind::GemlinkServerTrace, &mut args)
            }
            "--gemlink-log" => attach(&mut sessions, SourceKind::GemlinkServerLog, &mut args),
            "--vrlink-log" => attach(&mut sessions, SourceKind::VrlinkClientLog, &mut args),
            "--vrlink-trace" => attach(&mut sessions, SourceKind::VrlinkServerTrace, &mut args),
            "--out" => out = args.next(),
            other => {
                eprintln!("unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    if sessions.is_empty() {
        eprintln!("no sessions given; see --software");
        std::process::exit(2);
    }
    Args { sessions, out }
}

fn attach(
    sessions: &mut [(String, Vec<SourceFile>)],
    kind: SourceKind,
    args: &mut impl Iterator<Item = String>,
) {
    let path = args.next().unwrap_or_else(|| {
        eprintln!("source flag requires a path");
        std::process::exit(2);
    });
    match sessions.last_mut() {
        Some((_, files)) => files.push(SourceFile { kind, path }),
        None => {
            eprintln!("source flag before any --software");
            std::process::exit(2);
        }
    }
}

fn load_source(kind: SourceKind, path: &str) -> anyhow::Result<Vec<MetricSamples>> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    Ok(match kind {
        SourceKind::GemlinkClientCsv => adapters::gemlink_client_csv(&contents),
        SourceKind::GemlinkServerTrace => adapters::gemlink_server_trace(&contents),
        SourceKind::GemlinkServerLog => adapters_server_log::gemlink_server_log(&contents),
        SourceKind::VrlinkClientLog => adapters::vrlink_client_log(&contents),
        SourceKind::VrlinkServerTrace => adapters::vrlink_client_log(&contents), // same line shapes
    })
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let mut sessions: Vec<Session> = Vec::new();

    for (software, files) in &args.sessions {
        let mut metrics: BTreeMap<Metric, Vec<f64>> = BTreeMap::new();
        let mut sources = Vec::new();
        for file in files {
            for samples in load_source(file.kind, &file.path)? {
                metrics
                    .entry(samples.metric)
                    .or_default()
                    .extend(samples.samples);
            }
            sources.push(file.path.clone());
        }
        let metric_map: BTreeMap<String, MetricDistribution> = metrics
            .into_iter()
            .map(|(metric, samples)| {
                (
                    metric.name().to_string(),
                    MetricDistribution::from_samples(metric.unit(), samples),
                )
            })
            .collect();
        sessions_software(&mut sessions, software.clone(), sources, metric_map);
    }

    let report = render(&sessions);
    println!("{report}");
    if let Some(path) = &args.out {
        std::fs::write(path, &report).map_err(|e| anyhow::anyhow!("cannot write {path}: {e}"))?;
        println!("written to {path}");
    }
    Ok(())
}

// Session is built field-by-field because Session's metrics map is keyed by name while the
// accumulation is keyed by Metric — this is where the two meet.
fn sessions_software(
    sessions: &mut Vec<Session>,
    software: String,
    sources: Vec<String>,
    metric_map: BTreeMap<String, MetricDistribution>,
) {
    sessions.push(Session {
        software,
        sources,
        metrics: metric_map,
    });
}

fn render(sessions: &[Session]) -> String {
    let mut report = String::new();
    report.push_str("# Link software comparison\n\n");
    report
        .push_str("Generated by x-compare. Every number is the software's own self-measurement, ");
    report.push_str(
        "normalized here. Metrics marked *(not cross-comparable)* have semantics private to ",
    );
    report.push_str("the software that printed them — they are context, not a scoreboard.\n\n");

    // All metrics any session carries, in schema order.
    let all_metrics: Vec<Metric> = [
        Metric::Fps,
        Metric::ThroughputMbps,
        Metric::M2pSelfReportedMs,
        Metric::DecodeP50Ms,
        Metric::DecodeP95Ms,
        Metric::ServerPipelineUs,
        Metric::VrlinkPreHMs,
        Metric::VrlinkPreCMs,
        Metric::VrlinkDeadLMs,
        Metric::FecFailures,
        Metric::PacketsLost,
        Metric::AudioUnderruns,
        Metric::ReadCeilingPerSec,
        Metric::DeliveryBudgetPerSec,
    ]
    .to_vec();

    // Header
    report.push_str("| metric |");
    for session in sessions {
        report.push_str(&format!(" {} |", session.software));
    }
    report.push_str(" comparable |\n");
    report.push_str(&format!("|---|{}---|\n", "---|".repeat(sessions.len())));

    for metric in all_metrics {
        let name = metric.name();
        let row_cells: Vec<String> = sessions
            .iter()
            .map(|session| match session.metrics.get(name) {
                Some(dist) => dist.row(),
                None => "—".into(),
            })
            .collect();
        let any_present = row_cells.iter().any(|cell| cell != "—");
        if !any_present {
            continue;
        }
        report.push_str(&format!("| `{name}` |"));
        for cell in &row_cells {
            report.push_str(&format!(" {cell} |"));
        }
        report.push_str(&format!(
            " {} |\n",
            if metric.comparable_across_software() {
                "yes"
            } else {
                "*(not cross-comparable)*"
            }
        ));
    }

    report.push_str("\n## Sources\n\n");
    for session in sessions {
        report.push_str(&format!(
            "- **{}**: {}\n",
            session.software,
            session.sources.join(", ")
        ));
    }

    report.push_str("\n## Reading this honestly\n\n");
    report.push_str(
        "- `m2p_self_reported_ms` is motion-to-photon *as the software measures it*, device-clock ",
    );
    report.push_str(
        "(input sample → submit → predicted vsync). It is the metric GemLink optimizes; a future ",
    );
    report.push_str(
        "stack is comparable only if it measures the same way. The display's response time is the ",
    );
    report.push_str("known constant the reader adds.\n");
    report.push_str(
        "- `decode_p50/p95_ms` is the decode stage on the same clock (packet arrival → decoded).\n",
    );
    report.push_str(
        "- vrlink's `vrlink_preh/prec/deadl_ms` are its own internal stage estimates; they are ",
    );
    report.push_str("context about what Valve tracks, not comparable latencies.\n");
    report.push_str(
        "- Loss and underrun counters are cumulative for the session; compare deltas over the ",
    );
    report.push_str("same duration, not raw totals.\n");

    report
}
