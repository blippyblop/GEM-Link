//! `bench` — single-entrypoint CLI: one command per experiment.
//!
//! ```text
//! bench run <scenario> [--seed N] [--iterations N] [--out DIR]
//! bench compare <golden.json> <candidate.json> [--gate key=+X% | key=ABS]...
//! bench scenarios
//! ```

#![forbid(unsafe_code)]

use std::process::ExitCode;
use x_bench::{
    RunMetrics, compare, gate, parse_gate, run_loopback, scenario, scenarios, write_run,
};

fn usage() -> String {
    "usage:\n\
     bench run <scenario> [--seed N] [--iterations N] [--out DIR]\n\
     bench compare <golden.json> <candidate.json> [--gate key=+10%]...\n\
     bench gate --run <metrics.json> [--goldens DIR]\n\
     bench scenarios"
        .into()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match real_main(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(report) => {
            eprintln!("{report}");
            ExitCode::FAILURE
        }
    }
}

fn real_main(args: &[String]) -> Result<(), String> {
    let cmd = args.first().ok_or_else(usage)?;

    match cmd.as_str() {
        "scenarios" => {
            for s in scenarios() {
                println!(
                    "{:22} profile={:16} gating={}",
                    s.name, s.profile.name, s.gating
                );
            }
            Ok(())
        }
        "run" => {
            let name = args
                .get(1)
                .ok_or_else(|| format!("{}\nmissing scenario", usage()))?;
            let scenario = scenario(name)
                .ok_or_else(|| format!("unknown scenario {name:?} (try `bench scenarios`)"))?;
            let mut seed = 42u64;
            let mut iterations = 100u32;
            let mut out = std::path::PathBuf::from("runs");
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--seed" => {
                        seed = args
                            .get(i + 1)
                            .and_then(|v| v.parse().ok())
                            .ok_or_else(|| "--seed needs a number".to_string())?;
                        i += 2;
                    }
                    "--iterations" => {
                        iterations = args
                            .get(i + 1)
                            .and_then(|v| v.parse().ok())
                            .filter(|&n: &u32| n >= 1)
                            .ok_or_else(|| "--iterations needs a number >= 1".to_string())?;
                        i += 2;
                    }
                    "--out" => {
                        out = args
                            .get(i + 1)
                            .map(std::path::PathBuf::from)
                            .ok_or_else(|| "--out needs a path".to_string())?;
                        i += 2;
                    }
                    other => return Err(format!("unknown flag {other:?}")),
                }
            }

            let metrics = run_loopback(&scenario, seed, iterations)?;
            let path = write_run(&out, &metrics).map_err(|e| format!("write failed: {e}"))?;
            println!(
                "scenario={} seed={} iterations={} codec={:?} fps={} bitrate={}Mbps \
                 p50={:.2}ms p95={:.2}ms p99={:.2}ms late={:.1}% opt={:.0}% events={}",
                metrics.scenario,
                metrics.seed,
                metrics.iterations,
                metrics.negotiation.codec,
                metrics.negotiation.fps,
                metrics.negotiation.bitrate_mbps,
                metrics.latency.p50_ms,
                metrics.latency.p95_ms,
                metrics.latency.p99_ms,
                metrics.delivery.missed_mandatory_pct,
                metrics.delivery.within_optimal_pct,
                metrics.events.len(),
            );
            println!("{}", path.display());
            Ok(())
        }
        "compare" => {
            let mut positional = Vec::new();
            let mut gates = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--gate" => {
                        let spec = args
                            .get(i + 1)
                            .ok_or_else(|| "--gate needs key=value".to_string())?;
                        gates.push(parse_gate(spec)?);
                        i += 2;
                    }
                    path => {
                        positional.push(path.to_string());
                        i += 1;
                    }
                }
            }
            if positional.len() != 2 {
                return Err(format!("{}\ncompare needs exactly two run files", usage()));
            }
            if gates.is_empty() {
                return Err("no gates given — add at least one --gate".to_string());
            }
            let load = |p: &String| -> Result<serde_json::Value, String> {
                let raw = std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?;
                serde_json::from_str(&raw).map_err(|e| format!("{p}: {e}"))
            };
            let golden = load(&positional[0])?;
            let candidate = load(&positional[1])?;
            compare(&golden, &candidate, &gates)
        }
        "gate" => {
            let mut run_path: Option<String> = None;
            let mut goldens = std::path::PathBuf::from("bench/goldens");
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--run" => {
                        run_path = Some(args.get(i + 1).cloned().ok_or("--run needs a path")?);
                        i += 2;
                    }
                    "--goldens" => {
                        goldens = args
                            .get(i + 1)
                            .map(std::path::PathBuf::from)
                            .ok_or("--goldens needs a path")?;
                        i += 2;
                    }
                    other => return Err(format!("unknown flag {other:?}")),
                }
            }
            let run_path = run_path.ok_or("gate needs --run <metrics.json>")?;
            let load = |p: &std::path::Path| -> Result<RunMetrics, String> {
                let raw =
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
                serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", p.display()))
            };
            let run = load(std::path::Path::new(&run_path))?;
            let golden_path = goldens.join(format!("{}-golden.json", run.scenario));
            let golden = load(&golden_path)?;
            gate(&run, &golden)
        }
        other => Err(format!("unknown command {other:?}\n{}", usage())),
    }
}
