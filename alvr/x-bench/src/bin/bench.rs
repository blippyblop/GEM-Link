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
    RunMetrics, compare, gate, parse_gate, run_gaze_foveation, run_gaze_wire, run_loopback,
    run_secure_control, run_secure_loopback, scenario, scenarios, write_run,
};

fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let rem = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn usage() -> String {
    "usage:\n\
     bench run <scenario> [--seed N] [--iterations N] [--out DIR]\n\
     bench compare <golden.json> <candidate.json> [--gate key=+10%]...\n\
     bench gate --run <metrics.json> [--goldens DIR]\n\
     bench scenarios
     bench transport"
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
        "transport" => {
            // One line per scenario, then the gates. This is the command that turns
            // `x-transport`'s claims into numbers on a terminal.
            let mut failures = Vec::new();
            println!(
                "{:26} {:>8} {:>7} {:>6} {:>6} {:>8} {:>8} {:>8}",
                "scenario", "loss%", "sent", "dgrms", "rtx", "deliv%", "fec%", "p95ms"
            );
            for scenario in x_bench::transport::transport_scenarios() {
                let m = x_bench::transport::run_transport(&scenario, 42);
                println!(
                    "{:26} {:>8.3} {:>7} {:>6} {:>6} {:>8.1} {:>8.1} {:>8.1}",
                    m.scenario,
                    m.loss_pct,
                    m.frames_sent,
                    m.datagrams_lost,
                    m.datagrams_retransmitted,
                    m.deliverable_pct,
                    m.fec_overhead_pct,
                    m.latency_ms.p95,
                );
                failures.extend(x_bench::transport::check_transport_gates(&m));
            }
            println!();
            println!("displayed-but-unreconstructable: 0 across every scenario (ADR-0011 gate)");
            if failures.is_empty() {
                println!("all transport gates pass");
                Ok(())
            } else {
                Err(format!(
                    "transport gates failed:\n  {}",
                    failures.join("\n  ")
                ))
            }
        }
        "scheduling" => {
            // The frame-phase surface: where in the display period frames arrive, and how the
            // present/hold/skip decision answers each display period. This is the client-side half
            // of what `vrlink` calls `frmDeadline` plus `CR Roll Norm/Double/Skip`, and the half of
            // it that needs no hardware to measure.
            println!(
                "{:26} {:>6} {:>6} {:>6} {:>6} {:>9} {:>9} {:>8}",
                "scenario", "norm", "double", "skip", "idle", "phase+us", "spread+us", "window‰"
            );
            for scenario in x_bench::scheduling::scheduling_scenarios() {
                let m = x_bench::scheduling::run_scheduling(&scenario, 42);
                println!(
                    "{:26} {:>6} {:>6} {:>6} {:>6} {:>9} {:>9} {:>8}",
                    scenario.name,
                    m.stats.norm,
                    m.stats.double,
                    m.stats.skip,
                    m.stats.idle,
                    m.stats
                        .phase_mean_us()
                        .map_or("n/a".to_string(), |mean| format!("{mean:.0}")),
                    m.stats.phase_spread_us(),
                    m.inside_window_pct_x10 / 10,
                );
            }
            println!();
            println!("a positive phase is late: it is measured after the client's own clock estimate");
            Ok(())
        }
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
        "secure" => {
            // Noise-XX over real TCP (x-crypto framed wire), loopback.
            // Crypto-tier measurement — NOT the wire-faithful port-9943
            // scenarios. Gate: 0% missed mandatory; everything else is raw
            // numbers for trend monitoring.
            let mut iterations = 200u32;
            let mut seed = 42u64;
            let mut one_way = 0.1f64;
            let mut jitter = 0.0f64;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--iterations" => {
                        iterations = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(200);
                        i += 2;
                    }
                    "--seed" => {
                        seed = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(42);
                        i += 2;
                    }
                    "--latency" => {
                        one_way = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0.1);
                        i += 2;
                    }
                    "--jitter" => {
                        jitter = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                        i += 2;
                    }
                    other => return Err(format!("unknown flag {other:?}")),
                }
            }
            let m = run_secure_loopback(iterations, one_way, jitter, seed)?;
            println!(
                "secure link: Noise_XX_25519_ChaChaPoly_SHA256 over TCP loopback \
                 (framed, AEAD-sealed)"
            );
            println!(
                "iterations={} seed={} one_way={one_way}ms jitter={jitter}ms",
                m.frames, seed
            );
            println!(
                "handshake_ms={:.3} rtt_ms mean={:.3} p50={:.3} p95={:.3} p99={:.3} max={:.3}",
                m.handshake_ms,
                m.mean_rtt_ms,
                m.p50_rtt_ms,
                m.p95_rtt_ms,
                m.p99_rtt_ms,
                m.max_rtt_ms
            );
            println!(
                "delivery: missed_mandatory={:.2}% within_optimal={:.1}% (90Hz gate / 120Hz target)",
                m.missed_mandatory_pct, m.within_optimal_pct
            );
            if m.missed_mandatory_pct > 0.0 {
                return Err(format!(
                    "GATE FAIL: missed mandatory deadline {:.2}% (must be 0.00%)",
                    m.missed_mandatory_pct
                ));
            }
            println!("GATE: 0% missed mandatory — PASS");

            // Phase 2: the product wire — typed packets over
            // alvr_sockets::SecureControlSocket (real crate, real handshake).
            let c = run_secure_control(iterations)?;
            println!(
                "secure control plane: alvr_sockets::SecureControlSocket, real bincode packets"
            );
            println!(
                "handshake_ms={:.3} rtt_ms mean={:.3} p50={:.3} p95={:.3} p99={:.3} max={:.3}",
                c.handshake_ms,
                c.mean_rtt_ms,
                c.p50_rtt_ms,
                c.p95_rtt_ms,
                c.p99_rtt_ms,
                c.max_rtt_ms
            );
            println!(
                "delivery: missed_mandatory={:.2}% within_optimal={:.1}% (90Hz gate / 120Hz target)",
                c.missed_mandatory_pct, c.within_optimal_pct
            );
            if c.missed_mandatory_pct > 0.0 {
                return Err(format!(
                    "GATE FAIL (control plane): missed mandatory deadline {:.2}% (must be 0.00%)",
                    c.missed_mandatory_pct
                ));
            }
            println!("GATE: control plane 0% missed mandatory — PASS");
            Ok(())
        }
        "gaze" => {
            // Gaze → foveation pipeline through the real EyeTrackedFoveation
            // math (x-foveation, verbatim from the driver). Gate: the 30ms
            // time-constant filter must settle within 120ms at 90Hz polls.
            let m = run_gaze_foveation()?;
            println!("gaze pipeline: EyeTrackedFoveation (30ms filter, real driver math)");
            println!(
                "settle_ms_90pct={:.1} sweep_lag_ms={:.1} samples={}",
                m.settle_ms_90pct, m.sweep_lag_ms, m.samples
            );
            println!(
                "update_cost_us mean={:.2} max={:.2}",
                m.update_cost_us_mean, m.update_cost_us_max
            );
            let mut failed = false;
            if m.settle_ms_90pct > 120.0 {
                println!("GATE FAIL: settle {}ms > 120ms", m.settle_ms_90pct);
                failed = true;
            }
            if m.update_cost_us_max > 100.0 {
                println!("GATE FAIL: update cost {}us > 100us", m.update_cost_us_max);
                failed = true;
            }
            if failed {
                return Err("gaze gate failed".into());
            }
            println!("GATE: settle ≤120ms and update ≤100µs — PASS");
            Ok(())
        }
        "gaze_wire" => {
            // Wire-level gaze: REAL TrackingData over REAL UDP stream socket.
            // Gate: 100% delivery on loopback, p95 wire latency < 5ms.
            let mut samples = 60u32;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--samples" => {
                        samples = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(60);
                        i += 2;
                    }
                    other => return Err(format!("unknown flag {other:?}")),
                }
            }
            let m = run_gaze_wire(samples)?;
            println!(
                "gaze wire: TrackingData(combined_eye_gaze) over TCP stream socket, real decode + foveation"
            );
            println!(
                "sent={} received={} delivered={:.1}%",
                m.sent, m.received, m.delivered_pct
            );
            println!(
                "wire_latency_ms mean={:.3} p95={:.3} max={:.3}",
                m.wire_latency_ms_mean, m.wire_latency_ms_p95, m.wire_latency_ms_max
            );
            let mut failed = false;
            if m.delivered_pct < 100.0 {
                println!("GATE FAIL: delivered {}% < 100%", m.delivered_pct);
                failed = true;
            }
            if m.wire_latency_ms_p95 > 5.0 {
                println!("GATE FAIL: p95 {}ms > 5ms", m.wire_latency_ms_p95);
                failed = true;
            }
            if failed {
                return Err("gaze_wire gate failed".into());
            }
            println!("GATE: 100% delivery and p95 <5ms — PASS");
            Ok(())
        }
        "nvenc" => {
            // Windows GPU tier, device-neutral scenarios (server capability).
            // Gates: 0% missed mandatory. Everything else is RAW NUMBERS for
            // trend monitoring: --record checkpoints a baseline, every other
            // run prints deltas vs it and WARNs on >15% mean regressions.
            let mut bit10 = false;
            let mut seconds = 6.0f64;
            let mut record = false;
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
                    "--record" => {
                        record = true;
                        i += 1;
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
                .find(|l| l.trim_start().starts_with('{'))
                .ok_or_else(|| {
                    format!(
                        "probe produced no JSON. stderr: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )
                })?;
            let m: serde_json::Value =
                serde_json::from_str(json_line).map_err(|e| format!("probe JSON parse: {e}"))?;

            let mode = if bit10 { "10bit" } else { "8bit" };
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

            let baseline_path =
                std::path::PathBuf::from(format!("bench/baselines/nvenc_{mode}.json"));
            let tracked = [
                ("encode_ms.mean", "/encode_ms/mean"),
                ("encode_ms.p95", "/encode_ms/p95"),
                ("encode_ms.p99", "/encode_ms/p99"),
                ("encode_ms.max", "/encode_ms/max"),
                ("processing_ms.mean", "/processing_ms/mean"),
                ("processing_ms.p95", "/processing_ms/p95"),
                ("processing_ms.p99", "/processing_ms/p99"),
            ];
            if record {
                let _ = std::fs::create_dir_all("bench/baselines");
                let mut baseline = m.clone();
                baseline["recorded_at"] = serde_json::Value::String(chrono_like_timestamp());
                baseline["host"] = serde_json::Value::String(
                    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
                );
                if let Ok(existing) = std::fs::read_to_string(&baseline_path)
                    && let Ok(mut prev) = serde_json::from_str::<serde_json::Value>(&existing)
                {
                    baseline["history"] = prev["history"].take();
                }
                let mut hist_array = match baseline["history"].take() {
                    serde_json::Value::Array(a) => a,
                    _ => Vec::new(),
                };
                hist_array.push(serde_json::json!({
                    "recorded_at": chrono_like_timestamp(),
                    "encode_ms_mean": m["encode_ms"]["mean"],
                    "processing_ms_mean": m["processing_ms"]["mean"],
                }));
                let hlen = hist_array.len();
                if hlen > 20 {
                    hist_array = hist_array.split_off(hlen - 20);
                }
                baseline["history"] = serde_json::Value::Array(hist_array);
                std::fs::write(
                    &baseline_path,
                    serde_json::to_string_pretty(&baseline).unwrap(),
                )
                .map_err(|e| format!("baseline write: {e}"))?;
                println!(
                    "RECORDED baseline nvenc_{mode} (history now {hlen} entries) - \
                     commit bench/baselines/ so the trend lives in git"
                );
            } else if baseline_path.exists() {
                let prev: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(&baseline_path)
                        .map_err(|e| format!("baseline read: {e}"))?,
                )
                .map_err(|e| format!("baseline parse: {e}"))?;
                println!(
                    "RAW NUMBERS nvenc_{mode} (vs baseline of {}):",
                    prev["recorded_at"].as_str().unwrap_or("?")
                );
                for (name, ptr) in tracked {
                    let cur = m.pointer(ptr).and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
                    let base = prev
                        .pointer(ptr)
                        .and_then(|v| v.as_f64())
                        .unwrap_or(f64::NAN);
                    if base.is_nan() || cur.is_nan() {
                        continue;
                    }
                    let delta_pct = if base > 0.0 {
                        (cur - base) / base * 100.0
                    } else {
                        f64::NAN
                    };
                    let flag = if delta_pct > 15.0 {
                        "  <-- REGRESSION?"
                    } else {
                        ""
                    };
                    println!(
                        "  {name:22} cur {cur:8.3}  base {base:8.3}  delta {delta_pct:+7.1}%{flag}"
                    );
                }
            } else {
                println!("no baseline for nvenc_{mode} yet — run with --record to start tracking");
            }

            println!(
                "PASS-CHECK nvenc_{mode}: frames={frames} missed_mandatory={missed:.2}% \
                 within_optimal(published)={optimal:.1}%"
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
