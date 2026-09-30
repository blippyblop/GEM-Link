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
        "nvenc" => {
            // Windows GPU tier: wraps nvenc_probe (feeder mode = no desktop
            // needed). Gates: 0% missed mandatory; the 120 Hz optimal target
            // is published, not enforced.
            let mut bit10 = false;
            let mut seconds = 6.0f64;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--bit10" => {
                        bit10 = true;
                        i += 1;
                    }
                    "--seconds" => {
                        seconds = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(6.0);
                        i += 2;
                    }
                    other => return Err(format!("unknown flag {other:?}")),
                }
            }
            let exe = [
                "target/release/nvenc_probe.exe",
                "target/debug/nvenc_probe.exe",
            ]
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .ok_or("nvenc_probe.exe not found — build it first (x-dda)")?;

            let out = std::process::Command::new(exe)
                .args([
                    "--feeder",
                    "--seconds",
                    &seconds.to_string(),
                    "--fps",
                    "120",
                ])
                .args(if bit10 { vec!["--bit10"] } else { vec![] })
                .output()
                .map_err(|e| format!("probe spawn failed: {e}"))?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let json_line = stdout
                .lines()
                .rev()
                .find(|l| l.trim_start().starts('{'))
                .ok_or_else(|| {
                    format!(
                        "probe produced no JSON. stderr: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )
                })?;
            let m: serde_json::Value =
                serde_json::from_str(json_line).map_err(|e| format!("probe JSON parse: {e}"))?;

            let missed = m["delivery_on_processing"]["missed_mandatory_pct"]
                .as_f64()
                .ok_or("probe JSON missing delivery stats")?;
            let optimal = m["delivery_on_processing"]["within_optimal_pct"]
                .as_f64()
                .unwrap_or(0.0);
            let frames = m["frames"].as_u64().unwrap_or(0);
            let mut failures = Vec::new();
            if frames == 0 {
                failures.push("no frames encoded".into());
            }
            if missed > 0.0 {
                failures.push(format!("{missed:.2}% frames missed the mandatory deadline"));
            }
            println!(
                "PASS-CHECK nvenc_{}: frames={frames} missed_mandatory={missed:.2}%                  within_optimal(target, published)={optimal:.1}%",
                if bit10 { "10bit" } else { "8bit" }
            );
            if failures.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{} gate(s) failed: {}",
                    failures.len(),
                    failures.join("; ")
                ))
            }
        }
        other => Err(format!("unknown command {other:?}\n{}", usage())),
    }
}
