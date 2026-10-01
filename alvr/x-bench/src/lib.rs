//! x-bench — GemLink's measurement harness.
//!
//! Phase 0 walk-skeleton: **deterministic loopback scenarios** that exercise the real
//! control-plane wire protocol by driving upstream's own socket and packet crates
//! (conformance by construction — the fake headset speaks authentic framing because it
//! *is* the authentic framing), under link-impairment profiles that model measured
//! device-link behaviors.
//!
//! Charter value "measured, not vibes": every roadmap change lands with a scenario
//! here and a gate in CI (`bench compare`).

#![forbid(unsafe_code)]

use alvr_packets::{ClientControlPacket, ServerControlPacket};
use alvr_sockets::{PeerType, ProtoControlSocket};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use x_protocol::{ClientCapabilities, ServerCapabilities, SessionPlan, negotiate, samples};

pub const SCHEMA_VERSION: u32 = 4;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const LOSS_PENALTY_MS: f64 = 12.0;

// ---------------------------------------------------------------------------
// Deterministic RNG (no external dependency; seeded, reproducible)

/// Tiny LCG; good enough for reproducible jitter/stall scheduling.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------------------------------------------------------------------------
// Impairment profiles — modeled on measured link behaviors of the target
// device class (dedicated 6 GHz AP topology, USB-C NCM tether, watchdog churn).

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct StallModel {
    pub every_n_exchanges: u32,
    pub duration_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImpairmentProfile {
    pub name: String,
    pub description: String,
    pub one_way_latency_ms: f64,
    pub jitter_ms: f64,
    pub loss_pct: f64,
    pub bandwidth_mbps: u32,
    pub stall: Option<StallModel>,
}

impl ImpairmentProfile {
    /// Client-side response delay for one exchange (deterministic given seed).
    fn client_delay_ms(&self, rng: &mut Lcg) -> f64 {
        let jitter = (rng.next_f64() * 2.0 - 1.0) * self.jitter_ms / 2.0;
        (self.one_way_latency_ms + jitter).max(0.0)
    }
}

pub fn profiles() -> Vec<ImpairmentProfile> {
    vec![
        ImpairmentProfile {
            name: "ncm_wired".into(),
            description: "USB-C NCM tether — near-deterministic micro-latency path".into(),
            one_way_latency_ms: 2.0,
            jitter_ms: 0.3,
            loss_pct: 0.0,
            bandwidth_mbps: 400,
            stall: None,
        },
        ImpairmentProfile {
            name: "wifi7_160_clean".into(),
            description: "Wi-Fi 7 / 6 GHz / 160 MHz, strong signal, dedicated airtime".into(),
            one_way_latency_ms: 8.0,
            jitter_ms: 2.0,
            loss_pct: 0.1,
            bandwidth_mbps: 1200,
            stall: None,
        },
        ImpairmentProfile {
            name: "wifi7_regrace".into(),
            description: "regulatory race degradation — capped TX power settles firmware \
                          rate control low; marginal-link latency with occasional loss"
                .into(),
            one_way_latency_ms: 25.0,
            jitter_ms: 15.0,
            loss_pct: 0.5,
            bandwidth_mbps: 150,
            stall: None,
        },
        ImpairmentProfile {
            name: "cqm_churn".into(),
            description: "connection-quality watchdog churn — channel hops and transient \
                          stalls on a degraded wireless link"
                .into(),
            one_way_latency_ms: 20.0,
            jitter_ms: 25.0,
            loss_pct: 1.0,
            bandwidth_mbps: 300,
            stall: Some(StallModel {
                every_n_exchanges: 25,
                duration_ms: 30.0,
            }),
        },
        ImpairmentProfile {
            name: "wifi6_lan".into(),
            description: "shared home-LAN Wi-Fi 6 — the Tier-2 device baseline".into(),
            one_way_latency_ms: 12.0,
            jitter_ms: 4.0,
            loss_pct: 0.2,
            bandwidth_mbps: 350,
            stall: None,
        },
    ]
}

pub fn profile(name: &str) -> Option<ImpairmentProfile> {
    profiles().into_iter().find(|p| p.name == name)
}

// ---------------------------------------------------------------------------
// Scenarios — (profile × device) pairs. Gating scenarios (ADR-0004: Steam Frame)
// block merges; informational scenarios record Tier-2 behavior.

/// Per-frame presentation budgets. **90 Hz (11.11 ms) is the MANDATORY
/// deadline — the gate. 120 Hz (8.33 ms) is the OPTIMAL deadline — the
/// published target: tracked and trended, not enforced.** The primary metric
/// is never the average: it is how many frames blow a deadline and by how
/// much. A beautiful mean with 1% late frames is a broken experience.
pub const MANDATORY_DEADLINE_MS: f64 = 1000.0 / 90.0;
pub const OPTIMAL_DEADLINE_MS: f64 = 1000.0 / 120.0;

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: &'static str,
    pub profile: ImpairmentProfile,
    pub client: ClientCapabilities,
    pub server: ServerCapabilities,
    pub gating: bool,
    /// Mandatory per-frame budget (gate): 90 Hz cadence.
    pub deadline_ms: f64,
    /// Optimal per-frame budget (target): 120 Hz cadence.
    pub optimal_ms: f64,
}

pub fn scenarios() -> Vec<Scenario> {
    let (frame, pro, q3) = (
        samples::steam_frame(),
        samples::quest_pro(),
        samples::quest_3(),
    );
    let mk = |name: &'static str,
              profile_name: &str,
              mut client: ClientCapabilities,
              gating: bool|
     -> Scenario {
        client.hostname = name.into();
        Scenario {
            name,
            profile: profile(profile_name).expect("static profile name"),
            client,
            server: samples::server(),
            gating,
            deadline_ms: MANDATORY_DEADLINE_MS,
            optimal_ms: OPTIMAL_DEADLINE_MS,
        }
    };
    vec![
        mk("frame_ncm", "ncm_wired", frame.clone(), true),
        mk("frame_wifi7_160", "wifi7_160_clean", frame.clone(), true),
        mk("frame_wifi7_regrace", "wifi7_regrace", frame.clone(), false),
        mk("frame_cqm_churn", "cqm_churn", frame, false),
        mk("quest_pro_wifi6", "wifi6_lan", pro, false),
        mk("quest3_wifi6", "wifi6_lan", q3, false),
    ]
}

pub fn scenario(name: &str) -> Option<Scenario> {
    scenarios().into_iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// Metrics schema (normalized, machine-readable)

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatencyStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub mean_ms: f64,
    pub max_ms: f64,
}

/// Delivery consistency against the per-frame deadline. These are the gates
/// that matter: an average says nothing if 1% of frames arrive an order of
/// magnitude late.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DeliveryStats {
    pub mandatory_ms: f64,
    pub optimal_ms: f64,
    /// % of samples that missed the MANDATORY deadline. Gate: 0.0 for
    /// gating scenarios.
    pub missed_mandatory_pct: f64,
    /// % of samples that met the OPTIMAL deadline. Tracked, trended,
    /// published \u{2014} not enforced.
    pub within_optimal_pct: f64,
    /// Worst overshoot past the mandatory deadline (0 if none missed).
    pub max_lateness_ms: f64,
    /// % of samples late by more than a full extra frame (mandatory * 2).
    pub late_over_1frame_pct: f64,
    /// Longest consecutive run of samples within the mandatory deadline
    /// (hitch detector).
    pub best_streak: usize,
}

pub fn delivery_stats(samples: &[f64], mandatory_ms: f64, optimal_ms: f64) -> DeliveryStats {
    let pct = |n: usize| -> f64 {
        if samples.is_empty() {
            0.0
        } else {
            n as f64 / samples.len() as f64 * 100.0
        }
    };
    let missed = samples.iter().filter(|s| **s > mandatory_ms).count();
    let within_opt = samples.iter().filter(|s| **s <= optimal_ms).count();
    let over1 = samples.iter().filter(|s| **s > mandatory_ms * 2.0).count();
    let max_lateness = samples
        .iter()
        .map(|s| (s - mandatory_ms).max(0.0))
        .fold(0.0, f64::max);
    let mut best_streak = 0usize;
    let mut streak = 0usize;
    for s in samples {
        if *s <= mandatory_ms {
            streak += 1;
            best_streak = best_streak.max(streak);
        } else {
            streak = 0;
        }
    }
    DeliveryStats {
        mandatory_ms,
        optimal_ms,
        missed_mandatory_pct: pct(missed),
        within_optimal_pct: pct(within_opt),
        max_lateness_ms: max_lateness,
        late_over_1frame_pct: pct(over1),
        best_streak,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchEvent {
    pub t_ms: f64,
    pub kind: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunMetrics {
    pub schema_version: u32,
    pub scenario: String,
    pub seed: u64,
    pub iterations: u32,
    pub gating: bool,
    pub negotiation: SessionPlan,
    pub connect_ms: f64,
    pub latency: LatencyStats,
    pub delivery: DeliveryStats,
    pub events: Vec<BenchEvent>,
    pub profile: ImpairmentProfile,
    pub gemlink_rev: String,
}

pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[idx - 1]
}

pub fn latency_stats(mut samples: Vec<f64>) -> LatencyStats {
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len() as f64;
    LatencyStats {
        p50_ms: percentile(&samples, 0.50),
        p95_ms: percentile(&samples, 0.95),
        p99_ms: percentile(&samples, 0.99),
        mean_ms: samples.iter().sum::<f64>() / n.max(1.0),
        max_ms: samples.last().copied().unwrap_or(0.0),
    }
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Loopback run — fake headset (listens, authentic client role) ↔ bench server
// (connects out, authentic server role). The measured sample per exchange is
// send→reply RTT, which includes the client's profile-delayed response.

/// One loopback at a time: the fake headset listens on the protocol's real
/// well-known port (9943 — clients always listen there, servers always connect
/// out to it), so concurrent loopbacks on one machine would collide.
pub fn run_loopback(scenario: &Scenario, seed: u64, iterations: u32) -> Result<RunMetrics, String> {
    // Windows loopback quirk: a fresh bind/listen on 9943 can collide with
    // TIME_WAIT remnants of the previous run (children share the local port;
    // Windows holds them ~4min) and surface as an immediate RST (10054 /
    // 10057) instead of a bind failure — even with SO_REUSEADDR. The hostile
    // window can outlive an instant retry, so back off before each retry.
    // Same seed reproduces identical metrics; retries keep gates honest.
    let mut attempt_err = String::new();
    for cooldown_ms in [0u64, 100, 500] {
        if cooldown_ms > 0 {
            std::thread::sleep(Duration::from_millis(cooldown_ms));
        }
        match run_loopback_inner(scenario, seed, iterations) {
            Ok(m) => return Ok(m),
            Err(e) if e.contains("10054") || e.contains("10057") => {
                attempt_err = if attempt_err.is_empty() {
                    e
                } else {
                    format!("{attempt_err} | retry(+{cooldown_ms}ms): {e}")
                };
            }
            Err(e) => return Err(e),
        }
    }
    Err(attempt_err)
}

fn run_loopback_inner(
    scenario: &Scenario,
    seed: u64,
    iterations: u32,
) -> Result<RunMetrics, String> {
    assert!(iterations >= 1, "iterations must be >= 1");

    let negotiation = negotiate(&scenario.client, &scenario.server)
        .map_err(|e| format!("negotiation failed for {}: {e}", scenario.name))?;

    let listener = alvr_sockets::get_server_listener(IO_TIMEOUT).map_err(es)?;

    let profile = scenario.profile.clone();
    let headset = std::thread::spawn(move || -> Result<u32, String> {
        let (mut socket, _) =
            ProtoControlSocket::connect_to(IO_TIMEOUT, PeerType::Server(&listener)).map_err(es)?;
        let mut rng = Lcg::new(seed ^ 0xDEAD_BEEF);
        let mut served = 0u32;
        while served < iterations {
            if let ServerControlPacket::KeepAlive =
                socket.recv::<ServerControlPacket>(IO_TIMEOUT).map_err(es)?
            {
                let delay = profile.client_delay_ms(&mut rng);
                std::thread::sleep(Duration::from_micros((delay * 1000.0) as u64));
                socket.send(&ClientControlPacket::KeepAlive).map_err(es)?;
                served += 1;
            }
        }
        Ok(served)
    });

    // Bench server: connect out to the headset, then ping-pong keepalives.
    let t_connect = Instant::now();
    let (mut server, _) = ProtoControlSocket::connect_to(
        IO_TIMEOUT,
        PeerType::AnyClient(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    )
    .map_err(es)?;
    let connect_ms = t_connect.elapsed().as_secs_f64() * 1000.0;

    let mut rng = Lcg::new(seed);
    let mut samples = Vec::with_capacity(iterations as usize);
    let mut events = Vec::new();
    let t0 = Instant::now();

    for i in 0..iterations {
        if let Some(stall) = scenario.profile.stall
            && (i + 1) % stall.every_n_exchanges == 0
        {
            std::thread::sleep(Duration::from_micros((stall.duration_ms * 1000.0) as u64));
            events.push(BenchEvent {
                t_ms: t0.elapsed().as_secs_f64() * 1000.0,
                kind: "stall".into(),
                detail: format!("watchdog churn +{}ms", stall.duration_ms),
            });
        }
        if rng.next_f64() * 100.0 < scenario.profile.loss_pct {
            events.push(BenchEvent {
                t_ms: t0.elapsed().as_secs_f64() * 1000.0,
                kind: "loss".into(),
                detail: format!("retransmit penalty +{LOSS_PENALTY_MS}ms"),
            });
        }
        let jitter = (rng.next_f64() * 2.0 - 1.0) * scenario.profile.jitter_ms / 2.0;
        let pre_delay = (scenario.profile.one_way_latency_ms + jitter).max(0.0);
        std::thread::sleep(Duration::from_micros((pre_delay * 1000.0) as u64));

        let start = Instant::now();
        server.send(&ServerControlPacket::KeepAlive).map_err(es)?;
        let reply = server.recv::<ClientControlPacket>(IO_TIMEOUT);
        match reply.map_err(es)? {
            ClientControlPacket::KeepAlive => {}
            _ => return Err("unexpected packet during keepalive exchange".into()),
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let served = headset
        .join()
        .map_err(|e| format!("headset panicked: {e:?}"))??;
    if served != iterations {
        return Err(format!("headset served {served}/{iterations} exchanges"));
    }

    Ok(RunMetrics {
        schema_version: SCHEMA_VERSION,
        scenario: scenario.name.into(),
        seed,
        iterations,
        gating: scenario.gating,
        negotiation,
        connect_ms,
        latency: latency_stats(samples.clone()),
        delivery: delivery_stats(&samples, scenario.deadline_ms, scenario.optimal_ms),
        events,
        profile: scenario.profile.clone(),
        gemlink_rev: option_env!("GEMLINK_GIT_REV").unwrap_or("dev").into(),
    })
}

/// Write `runs/<scenario>-s<seed>-<unix_ms>/metrics.json`; returns the file path.
pub fn write_run(
    base_dir: &std::path::Path,
    metrics: &RunMetrics,
) -> std::io::Result<std::path::PathBuf> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dir = base_dir.join(format!("{}-s{}-{ms}", metrics.scenario, metrics.seed));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("metrics.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(metrics).expect("serialize metrics"),
    )?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Gates — `bench compare golden.json candidate.json --gate latency_ms.p50=+10%`

#[derive(Clone, Debug, PartialEq)]
pub enum GateSpec {
    /// Candidate may be at most X% worse than golden.
    Percent(f64),
    /// Candidate must be at most this absolute value.
    Absolute(f64),
}

pub fn parse_gate(spec: &str) -> Result<(String, GateSpec), String> {
    let (key, bound) = spec
        .split_once('=')
        .ok_or_else(|| format!("gate must look like key=+10% or key=25, got {spec:?}"))?;
    if bound.starts_with('+') {
        let pct = bound
            .strip_prefix('+')
            .and_then(|s| s.strip_suffix('%'))
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| format!("a '+' bound must be a percentage (key=+10%), got {spec:?}"))?;
        Ok((key.into(), GateSpec::Percent(pct)))
    } else if let Ok(v) = bound.parse::<f64>() {
        Ok((key.into(), GateSpec::Absolute(v)))
    } else {
        Err(format!("unparseable gate bound in {spec:?}"))
    }
}

fn lookup(value: &serde_json::Value, path: &str) -> Option<f64> {
    let mut cur = value;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    cur.as_f64()
}

/// Returns `Err` with a human-readable report when any gate fails.
pub fn compare(
    golden: &serde_json::Value,
    candidate: &serde_json::Value,
    gates: &[(String, GateSpec)],
) -> Result<(), String> {
    let mut failures = Vec::new();
    for (key, spec) in gates {
        let g = lookup(golden, key)
            .ok_or_else(|| format!("gate key {key:?} not found in golden run"))?;
        let c = lookup(candidate, key)
            .ok_or_else(|| format!("gate key {key:?} not found in candidate run"))?;
        let limit = match spec {
            GateSpec::Percent(p) => g * (1.0 + p / 100.0),
            GateSpec::Absolute(v) => *v,
        };
        let status = if c <= limit { "PASS" } else { "FAIL" };
        println!("{status} {key}: {c:.3} (golden {g:.3}, limit {limit:.3})");
        if c > limit {
            failures.push(format!("{key}: {c:.3} > {limit:.3}"));
        }
    }
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

// ---------------------------------------------------------------------------
// Golden gates — machine-independent CI checks: structural equality against a
// checked-in golden run, plus profile-derived latency sanity caps. Percent
// regression gating (`bench compare`) is for same-machine A/B runs.

/// Loose upper bound that only trips on catastrophic regressions (accidental
/// sleeps, retry storms), never on runner variance.
pub fn sanity_cap_ms(profile: &ImpairmentProfile) -> f64 {
    let stall = profile.stall.map(|s| s.duration_ms).unwrap_or(0.0);
    4.0 * profile.one_way_latency_ms
        + 4.0 * profile.jitter_ms
        + stall
        + 3.0 * LOSS_PENALTY_MS
        + 50.0
}

fn record(failures: &mut Vec<String>, name: &str, ok: bool, detail: String) {
    println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        failures.push(name.into());
    }
}

/// Gate a run against its golden: structure must match exactly, latency must
/// stay under the profile-derived sanity cap.
pub fn gate(run: &RunMetrics, golden: &RunMetrics) -> Result<(), String> {
    if run.scenario != golden.scenario {
        return Err(format!(
            "scenario mismatch: run={} golden={}",
            run.scenario, golden.scenario
        ));
    }
    let mut failures = Vec::new();
    record(
        &mut failures,
        "schema_version",
        run.schema_version == golden.schema_version,
        format!(
            "run {} / golden {}",
            run.schema_version, golden.schema_version
        ),
    );
    record(
        &mut failures,
        "seed",
        run.seed == golden.seed,
        format!("run {} / golden {}", run.seed, golden.seed),
    );
    record(
        &mut failures,
        "iterations",
        run.iterations == golden.iterations,
        format!("run {} / golden {}", run.iterations, golden.iterations),
    );
    record(
        &mut failures,
        "profile",
        run.profile == golden.profile,
        format!("run {} / golden {}", run.profile.name, golden.profile.name),
    );
    record(
        &mut failures,
        "negotiation",
        run.negotiation == golden.negotiation,
        format!(
            "run codec={:?} fps={} bitrate={} link={:?} / golden codec={:?} fps={} bitrate={} link={:?}",
            run.negotiation.codec,
            run.negotiation.fps,
            run.negotiation.bitrate_mbps,
            run.negotiation.link_class,
            golden.negotiation.codec,
            golden.negotiation.fps,
            golden.negotiation.bitrate_mbps,
            golden.negotiation.link_class
        ),
    );
    let events = |m: &RunMetrics| -> Vec<(String, String)> {
        m.events
            .iter()
            .map(|e| (e.kind.clone(), e.detail.clone()))
            .collect()
    };
    record(
        &mut failures,
        "event_sequence",
        events(run) == events(golden),
        format!(
            "run {} events / golden {} events (kind+detail sequence)",
            events(run).len(),
            events(golden).len()
        ),
    );
    let cap = sanity_cap_ms(&golden.profile);
    record(
        &mut failures,
        "latency_p99_sanity",
        run.latency.p99_ms <= cap,
        format!("p99 {:.2} ms / cap {cap:.2} ms", run.latency.p99_ms),
    );
    if run.gating {
        record(
            &mut failures,
            "missed_deadlines",
            run.delivery.missed_mandatory_pct == 0.0,
            format!(
                "gating scenario: {:.1}% missed the mandatory {:.2} ms deadline \
                 ({:.0}% within the optimal {:.2} ms target)",
                run.delivery.missed_mandatory_pct,
                run.delivery.mandatory_ms,
                run.delivery.within_optimal_pct,
                run.delivery.optimal_ms
            ),
        );
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} gate check(s) failed", failures.len()))
    }
}

use std::net::{TcpListener, TcpStream};
use x_crypto::{HandshakeRole, Identity};

// ---------------------------------------------------------------------------
// Secure loopback — Noise-XX over a real TCP pair, then AEAD-sealed
// exchanges under the same impairment profiles. This measures what
// encryption actually costs on the wire: handshake duration, sealed RTT,
// delivery vs the two-tier budgets. (Socket pair is ephemeral-port on
// loopback: this is a crypto-latency measurement, not protocol
// conformance — the wire-faithful scenarios use the well-known port.)

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecureRunMetrics {
    pub handshake_ms: f64,
    pub mean_rtt_ms: f64,
    pub p50_rtt_ms: f64,
    pub p95_rtt_ms: f64,
    pub p99_rtt_ms: f64,
    pub max_rtt_ms: f64,
    pub missed_mandatory_pct: f64,
    pub within_optimal_pct: f64,
    pub frames: u32,
}

pub fn run_secure_loopback(
    iterations: u32,
    one_way_latency_ms: f64,
    jitter_ms: f64,
    seed: u64,
) -> Result<SecureRunMetrics, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;

    // Deterministic jitter for both endpoints.
    let mut client_rng = Lcg::new(seed);
    let mut server_rng = Lcg::new(seed ^ 0x5EED_0000);

    let (err_tx, err_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        let (stream, _) = match listener.accept() {
            Ok(x) => x,
            Err(e) => {
                let _ = err_tx.send(e.to_string());
                return;
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            let _ = err_tx.send(e.to_string());
            return;
        }
        if let Err(e) = secure_server_side(
            stream,
            &mut server_rng,
            one_way_latency_ms,
            jitter_ms,
            iterations,
        ) {
            let _ = err_tx.send(e);
        }
    });

    let stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut handshake_ms = 0.0f64;
    let mut rtts: Vec<f64> = Vec::new();
    let client_result = secure_client_side(
        stream,
        &mut client_rng,
        one_way_latency_ms,
        jitter_ms,
        iterations,
        &mut handshake_ms,
        &mut rtts,
    );

    handle
        .join()
        .map_err(|_| "server side panicked".to_string())?;
    let server_err = err_rx.try_recv().ok();

    let client_err = client_result.err();
    if client_err.is_some() || server_err.is_some() {
        return Err(match (client_err, server_err) {
            (Some(ce), Some(se)) => format!("client: {ce} | server: {se}"),
            (Some(ce), None) => ce,
            (None, Some(se)) => se,
            (None, None) => unreachable!(),
        });
    }

    let ls = latency_stats(rtts.clone());
    let ds = delivery_stats(&rtts, MANDATORY_DEADLINE_MS, OPTIMAL_DEADLINE_MS);
    Ok(SecureRunMetrics {
        handshake_ms,
        mean_rtt_ms: ls.mean_ms,
        p50_rtt_ms: ls.p50_ms,
        p95_rtt_ms: ls.p95_ms,
        p99_rtt_ms: ls.p99_ms,
        max_rtt_ms: ls.max_ms,
        missed_mandatory_pct: ds.missed_mandatory_pct,
        within_optimal_pct: ds.within_optimal_pct,
        frames: iterations,
    })
}

/// Secure control-plane measurement: typed packets over
/// `alvr_sockets::SecureControlSocket` (real Noise-XX handshake + real
/// bincode control packets). This is the wire the product will speak.
pub fn run_secure_control(iterations: u32) -> Result<SecureRunMetrics, String> {
    use alvr_session::SocketBufferConfig;
    use alvr_sockets::SecureControlSocket;
    use std::net::TcpListener;
    use x_crypto::Identity;

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;

    let server_id = Identity::generate().map_err(|e| e.to_string())?;
    let server_handle = std::thread::spawn(move || -> Result<(), String> {
        let (server, _remote) = SecureControlSocket::accept_from_client(
            &listener,
            None,
            Duration::from_secs(5),
            &server_id,
        )
        .map_err(|e| e.to_string())?;
        for _ in 0..iterations {
            let _req: ClientControlPacket = server
                .recv(Duration::from_secs(2))
                .map_err(|e| e.to_string())?;
            server
                .send(&ServerControlPacket::KeepAlive)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    });

    let client_id = Identity::generate().map_err(|e| e.to_string())?;
    let hs_start = Instant::now();
    let (client, _remote) = SecureControlSocket::connect_to_server(
        Duration::from_secs(5),
        &[addr.ip()],
        addr.port(),
        SocketBufferConfig::default(),
        &client_id,
    )
    .map_err(|e| e.to_string())?;
    let handshake_ms = hs_start.elapsed().as_secs_f64() * 1000.0;

    let mut rtts = Vec::new();
    for _ in 0..iterations {
        let t0 = Instant::now();
        client
            .send(&ClientControlPacket::KeepAlive)
            .map_err(|e| e.to_string())?;
        let _ack: ServerControlPacket = client
            .recv(Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        rtts.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    server_handle
        .join()
        .map_err(|_| "server thread panicked".to_string())??;

    let ls = latency_stats(rtts.clone());
    let ds = delivery_stats(&rtts, MANDATORY_DEADLINE_MS, OPTIMAL_DEADLINE_MS);
    Ok(SecureRunMetrics {
        handshake_ms,
        mean_rtt_ms: ls.mean_ms,
        p50_rtt_ms: ls.p50_ms,
        p95_rtt_ms: ls.p95_ms,
        p99_rtt_ms: ls.p99_ms,
        max_rtt_ms: ls.max_ms,
        missed_mandatory_pct: ds.missed_mandatory_pct,
        within_optimal_pct: ds.within_optimal_pct,
        frames: iterations,
    })
}

/// Gaze → foveation pipeline: deterministic sweep through the REAL
/// `EyeTrackedFoveation` (extracted verbatim from the driver crate into
/// x-foveation). Measures what the flagship feature actually costs:
/// filter settling after a gaze step, steady-state lag during a sweep,
/// and per-sample update cost. Deterministic — no RNG, no wall clock
/// beyond measuring update cost.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GazeRunMetrics {
    pub settle_ms_90pct: f64,
    pub sweep_lag_ms: f64,
    pub update_cost_us_mean: f64,
    pub update_cost_us_max: f64,
    pub samples: u32,
}

pub fn run_gaze_foveation() -> Result<GazeRunMetrics, String> {
    use alvr_common::{
        AlvrFoveatedEncodingParams, Fov, Pose, ViewParams,
        glam::{Quat, UVec2, Vec3},
    };
    use std::time::Instant;
    use x_foveation::EyeTrackedFoveation;

    const POLL_HZ: f32 = 90.0;
    const HOLD_S: f32 = 0.3; // settle window before the step
    const SETTLE_WINDOW_S: f32 = 1.0;
    const SWEEP_S: f32 = 0.5; // constant-velocity sweep after the step
    const STEP_DEG: f32 = 15.0;
    const SWEEP_DEG: f32 = 30.0;

    // Frame-class FOV: ±55° horizontal, ±45° vertical.
    let view_params = [ViewParams {
        pose: Pose {
            orientation: Quat::IDENTITY,
            position: Vec3::ZERO,
        },
        fov: Fov {
            left: -55.0_f32.to_radians(),
            right: 55.0_f32.to_radians(),
            up: 45.0_f32.to_radians(),
            down: -45.0_f32.to_radians(),
        },
    }; 2];

    let params = AlvrFoveatedEncodingParams {
        encoded_view_resolution: [2048, 2048],
        view_ratio: [1.0, 1.0],
        center_size: [0.4, 0.4],
        center_shifts: [[0.0, 0.0]; 2],
        edge_ratio: [4.0, 4.0],
    };
    let mut foveation = EyeTrackedFoveation::new(params, UVec2::new(2048, 2048));
    foveation.view_params = Some(view_params);

    let poll_s = 1.0 / POLL_HZ;
    let mut t = 0.0f32;
    let mut centers_x: Vec<(f32, f32)> = Vec::new(); // (time_s, left center_shift_x)
    let mut update_costs = Vec::new();

    let step = |foveation: &mut EyeTrackedFoveation,
                t: f32,
                azimuth_deg: f32,
                costs: &mut Vec<f64>,
                out: &mut Vec<(f32, f32)>|
     -> Result<(), String> {
        let ts = Duration::from_secs_f32(t);
        let orientation = Quat::from_rotation_y(azimuth_deg.to_radians());
        let t0 = Instant::now();
        foveation.update(ts, Some(orientation), Instant::now());
        costs.push(t0.elapsed().as_secs_f64() * 1e6);
        let centers = foveation
            .centers(ts)
            .ok_or_else(|| format!("no centers at t={t}"))?;
        out.push((t, centers[0][0]));
        Ok(())
    };

    // Phase 1: hold center — initialize the filter.
    for i in 0..((HOLD_S / poll_s) as u32) {
        t = i as f32 * poll_s;
        step(&mut foveation, t, 0.0, &mut update_costs, &mut centers_x)?;
    }

    // Phase 2: STEP +15° — settling time to 90% of the total shift.
    let baseline = centers_x.last().unwrap().1;
    let mut settle_ms: Option<f64> = None;
    for i in 1..=((SETTLE_WINDOW_S / poll_s) as u32) {
        t = HOLD_S + i as f32 * poll_s;
        step(
            &mut foveation,
            t,
            STEP_DEG,
            &mut update_costs,
            &mut centers_x,
        )?;
        if settle_ms.is_none() {
            let current = centers_x.last().unwrap().1;
            let target = baseline + 0.9 * (current - baseline);
            let _ = target; // 90% criterion below uses the final value
        }
    }
    let settled = centers_x.last().unwrap().1;
    let total_shift = settled - baseline;
    for (sample_t, cx) in &centers_x {
        if *sample_t > HOLD_S && settle_ms.is_none() {
            // Magnitude criterion: works for either direction of the step.
            if ((cx - baseline) / total_shift).abs() >= 0.9 {
                settle_ms = Some(((sample_t - HOLD_S) * 1000.0) as f64);
            }
        }
    }
    let settle_ms_90pct = settle_ms.ok_or("gaze filter never settled within window")?;

    // Phase 3: constant-velocity sweep back to 0° — steady-state lag.
    // Invert the projection analytically: measured shift → input azimuth →
    // the input time that produced it → lag. Averaged over the ramp tail.
    let sweep_start = t;
    let sweep_rate = SWEEP_DEG / SWEEP_S; // deg/s
    let mut lags = Vec::new();
    for i in 1..=((SWEEP_S / poll_s) as u32) {
        t = sweep_start + i as f32 * poll_s;
        let azimuth = STEP_DEG - sweep_rate * (t - sweep_start);
        step(
            &mut foveation,
            t,
            azimuth,
            &mut update_costs,
            &mut centers_x,
        )?;
        if i > 10 {
            let measured = centers_x.last().unwrap().1;
            let az_in = azimuth_for_shift_x(measured);
            let input_t = sweep_start + (STEP_DEG - az_in) / sweep_rate;
            lags.push(((t - input_t) * 1000.0) as f64);
        }
    }
    let sweep_lag_ms = lags.iter().sum::<f64>() / lags.len().max(1) as f64;

    // Sanity: centers finite and inside the aligned bounds.
    for (_, cx) in &centers_x {
        if !cx.is_finite() || cx.abs() > 1.5 {
            return Err(format!("center shift out of bounds: {cx}"));
        }
    }

    // Untimed-warmup lesson (NVENC): the first calls pay one-time allocator
    // and page-fault costs — exclude them from the steady-state cost stats.
    let warm = update_costs.len().min(5);
    let steady = &update_costs[warm..];
    let update_cost_us_mean = steady.iter().sum::<f64>() / steady.len() as f64;
    let update_cost_us_max = steady.iter().cloned().fold(0.0, f64::max);
    Ok(GazeRunMetrics {
        settle_ms_90pct,
        sweep_lag_ms,
        update_cost_us_mean,
        update_cost_us_max,
        samples: centers_x.len() as u32,
    })
}

/// Wire-level gaze hop: the LAST untested link of the flagship path. The
/// fake headset encodes REAL `TrackingData` packets (combined_eye_gaze
/// quat, poll_timestamp) and fires them over a REAL UDP stream socket
/// (`alvr_sockets::StreamSocket`, stream id TRACKING). The receiver side
/// decodes, feeds the REAL `EyeTrackedFoveation`, and measures
/// send→centers latency. Everything on this path is the product wire.
/// (TCP stream variant: UDP's symmetric-port design — both ends bind the
/// same port number on their own machines — is untestable on one host.)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GazeWireMetrics {
    pub sent: u32,
    pub received: u32,
    pub delivered_pct: f64,
    pub wire_latency_ms_mean: f64,
    pub wire_latency_ms_p95: f64,
    pub wire_latency_ms_max: f64,
}

const GAZE_WIRE_HEADSET_PORT: u16 = 19443;
const GAZE_WIRE_PC_PORT: u16 = 19444;

pub fn run_gaze_wire(samples: u32) -> Result<GazeWireMetrics, String> {
    use alvr_common::{
        AlvrFoveatedEncodingParams, Fov, Pose, ViewParams,
        glam::{Quat, UVec2, Vec3},
    };
    use alvr_packets::{TRACKING, TrackingData};
    use alvr_session::{SocketBufferConfig, SocketProtocol};
    use alvr_sockets::StreamSocketBuilder;
    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use x_foveation::EyeTrackedFoveation;

    const POLL_HZ: f32 = 90.0;
    const HOLD_N: u32 = 27;
    const SWEEP_N: u32 = 45;
    const SWEEP_DEG: f32 = 30.0;
    const MAX_PACKET: usize = 1472;

    assert!(
        samples <= HOLD_N + SWEEP_N,
        "cap samples at the scripted sweep"
    );

    let headset_port = GAZE_WIRE_HEADSET_PORT;
    let pc_port = GAZE_WIRE_PC_PORT;
    let poll_s = 1.0 / POLL_HZ;
    let sweep_rate = SWEEP_DEG / ((SWEEP_N as f32) * poll_s); // deg/s

    let t_sent: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));

    // Fake headset: real stream socket listener, REAL TrackingData packets
    // with a deterministic gaze sweep at 90 Hz.
    let listener = StreamSocketBuilder::listen_for_server(
        Duration::from_secs(5),
        headset_port,
        SocketProtocol::Tcp,
        None,
        SocketBufferConfig::default(),
    )
    .map_err(|e| e.to_string())?;

    // PC side: real stream socket, subscribe TRACKING, decode, feed the real
    // foveation, record send→centers latency.
    let t_sent_pc = Arc::clone(&t_sent);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let pc_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let pc_errors_pc = Arc::clone(&pc_errors);
    let pc = std::thread::spawn(move || -> Result<(u32, Vec<f64>), String> {
        let log_err = |what: &str, e: String| {
            pc_errors_pc
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{what}: {e}"));
        };
        let mut socket = StreamSocketBuilder::connect_to_client(
            Duration::from_secs(5),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            headset_port,
            SocketProtocol::Tcp,
            None,
            SocketBufferConfig::default(),
            MAX_PACKET,
        )
        .map_err(|e| format!("pc connect: {e}"))?;
        let mut receiver = socket.subscribe_to_stream::<TrackingData>(TRACKING, 32);
        let _ = ready_tx.send(());

        let view_params = [ViewParams {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::ZERO,
            },
            fov: Fov {
                left: -55.0_f32.to_radians(),
                right: 55.0_f32.to_radians(),
                up: 45.0_f32.to_radians(),
                down: -45.0_f32.to_radians(),
            },
        }; 2];
        let params = AlvrFoveatedEncodingParams {
            encoded_view_resolution: [2048, 2048],
            view_ratio: [1.0, 1.0],
            center_size: [0.4, 0.4],
            center_shifts: [[0.0, 0.0]; 2],
            edge_ratio: [4.0, 4.0],
        };
        let mut foveation = EyeTrackedFoveation::new(params, UVec2::new(2048, 2048));
        foveation.view_params = Some(view_params);

        let mut latencies = Vec::new();
        let mut received = 0u32;
        while received < samples {
            match socket.recv() {
                Ok(()) => {}
                Err(e) if format!("{e}").contains("Try again") => continue,
                Err(e) => {
                    log_err("stream recv pump", format!("{e}"));
                    return Err(format!("stream recv pump: {e}"));
                }
            }
            loop {
                match receiver.recv(Duration::ZERO) {
                    Ok(data) => {
                        let (tracking, _) = data.get().map_err(|e| e.to_string())?;
                        let sent_at = t_sent_pc
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .pop_front()
                            .ok_or("received more packets than sent")?;
                        if let Some(gaze) = tracking.face.eyes_combined {
                            let now = Instant::now();
                            foveation.update(tracking.poll_timestamp, Some(gaze), now);
                            if foveation.centers(tracking.poll_timestamp).is_some() {
                                latencies.push(now.duration_since(sent_at).as_secs_f64() * 1000.0);
                            }
                        }
                        received += 1;
                    }
                    // Empty-queue timeouts (incl. recv(ZERO) racing the pump
                    // dispatch) mean "no packet yet" — repump, not fatal.
                    Err(e)
                        if format!("{e}").contains("timed out")
                            || format!("{e}").contains("Try again") =>
                    {
                        break;
                    }
                    Err(e) => {
                        log_err("tracking recv", format!("{e}"));
                        return Err(format!("tracking recv: {e}"));
                    }
                }
            }
        }
        Ok((received, latencies))
    });

    if ready_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let pc_err = pc
            .join()
            .map(|r| format!("{r:?}"))
            .unwrap_or_else(|_| "panicked".to_string());
        return Err(format!("PC side never became ready: {pc_err}"));
    }
    let headset_sock = listener
        .accept_from_server(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            pc_port,
            MAX_PACKET,
            Duration::from_secs(5),
        )
        .map_err(|e| e.to_string())?;
    let mut sender = headset_sock.request_stream::<TrackingData>(TRACKING);

    let mut sent = 0u32;
    for i in 0..samples {
        let t = i as f32 * poll_s;
        // Hold center, then a constant-velocity ramp (driver sign convention).
        let az = if i < HOLD_N {
            0.0
        } else {
            -((i - HOLD_N) as f32) * poll_s * sweep_rate
        };
        let packet = TrackingData {
            poll_timestamp: Duration::from_secs_f32(t),
            device_motions: vec![],
            hand_skeletons: [None, None],
            face: alvr_packets::FaceData {
                eyes_combined: Some(Quat::from_rotation_y(az.to_radians())),
                eyes_social: [None, None],
                face_expressions: None,
            },
            body: None,
        };
        t_sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(Instant::now());
        if let Err(e) = sender.send_header(&packet) {
            let errs = pc_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .join(" | ");
            return Err(format!("headset send #{sent}: {e} || pc errors: [{errs}]"));
        }
        sent += 1;
        std::thread::sleep(Duration::from_secs_f32(poll_s));
    }

    let (received, latencies) = pc.join().map_err(|_| "PC thread panicked".to_string())??;
    let _ = headset_sock;

    let ls = latency_stats(latencies.clone());
    Ok(GazeWireMetrics {
        sent,
        received,
        delivered_pct: if sent > 0 {
            received as f64 / sent as f64 * 100.0
        } else {
            0.0
        },
        wire_latency_ms_mean: ls.mean_ms,
        wire_latency_ms_p95: ls.p95_ms,
        wire_latency_ms_max: ls.max_ms,
    })
}

fn azimuth_for_shift_x(shift: f32) -> f32 {
    // Driver convention: yaw +az maps -Z toward -X, so tangent.x = -tan(az)
    // and the shift sign flips relative to the naive mirror.
    let lo = (-55.0_f32.to_radians()).tan();
    let hi = 55.0_f32.to_radians().tan();
    let uv = shift * 0.6 / 2.0 + 0.5;
    -(uv * (hi - lo) + lo).atan().to_degrees()
}

fn secure_client_side(
    stream: TcpStream,
    rng: &mut Lcg,
    one_way_latency_ms: f64,
    jitter_ms: f64,
    iterations: u32,
    handshake_ms_out: &mut f64,
    rtts_out: &mut Vec<f64>,
) -> Result<(), String> {
    use std::time::{Duration, Instant};
    use x_crypto::framed::NoiseSocket;

    let local = Identity::generate().map_err(|e| e.to_string())?;
    let hs_start = Instant::now();
    let (mut sock, _remote_static) =
        NoiseSocket::handshake(stream, HandshakeRole::Initiator, &local)
            .map_err(|e| format!("client handshake: {e}"))?;
    *handshake_ms_out = hs_start.elapsed().as_secs_f64() * 1000.0;
    // Real clients compare `_remote_static`'s fingerprint against the
    // pairing store here; the bench uses fresh ephemeral identities.

    let recv_timeout = Duration::from_secs_f64(
        ((2.0 * (one_way_latency_ms + jitter_ms)) + 250.0).max(300.0) / 1000.0,
    );
    for i in 0..iterations {
        let jitter = if jitter_ms > 0.0 {
            (rng.next_f64() * 2.0 - 1.0) * jitter_ms
        } else {
            0.0
        };
        std::thread::sleep(Duration::from_secs_f64(
            (one_way_latency_ms + jitter).max(0.0) / 1000.0,
        ));
        let t0 = Instant::now();
        let payload = format!("secure frame {i}");
        sock.send_frame(payload.as_bytes())
            .map_err(|e| e.to_string())?;
        let ack = sock.recv_frame(recv_timeout).map_err(|e| e.to_string())?;
        let _ = String::from_utf8_lossy(&ack);
        rtts_out.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(())
}

fn secure_server_side(
    stream: TcpStream,
    rng: &mut Lcg,
    one_way_latency_ms: f64,
    jitter_ms: f64,
    iterations: u32,
) -> Result<(), String> {
    use std::time::{Duration, Instant};
    use x_crypto::framed::NoiseSocket;

    let local = Identity::generate().map_err(|e| e.to_string())?;
    let (mut sock, _remote_static) =
        NoiseSocket::handshake(stream, HandshakeRole::Responder, &local)
            .map_err(|e| format!("server handshake: {e}"))?;

    let recv_timeout = Duration::from_secs_f64(
        ((2.0 * (one_way_latency_ms + jitter_ms)) + 250.0).max(300.0) / 1000.0,
    );
    for _ in 0..iterations {
        let jitter = if jitter_ms > 0.0 {
            (rng.next_f64() * 2.0 - 1.0) * jitter_ms
        } else {
            0.0
        };
        std::thread::sleep(Duration::from_secs_f64(
            (one_way_latency_ms + jitter).max(0.0) / 1000.0,
        ));
        let req = sock.recv_frame(recv_timeout).map_err(|e| e.to_string())?;
        let m = req.len();
        let reply = format!("ack-{m}");
        sock.send_frame(reply.as_bytes())
            .map_err(|e| e.to_string())?;
        let _ = Instant::now();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use x_protocol::VideoCodec;

    /// Loopbacks bind the real well-known port — serialize them.
    fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
        SERIAL
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn profiles_and_scenarios_are_consistent() {
        let ps = profiles();
        assert!(ps.len() >= 5);
        for s in scenarios() {
            assert!(
                negotiate(&s.client, &s.server).is_ok(),
                "{} must negotiate",
                s.name
            );
        }
        assert!(scenario("frame_ncm").unwrap().gating);
        assert!(!scenario("quest_pro_wifi6").unwrap().gating);
        assert!(scenario("nope").is_none());
    }

    #[test]
    fn percentile_math() {
        let v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.95), 95.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn lcg_is_deterministic() {
        let (mut a, mut b) = (Lcg::new(42), Lcg::new(42));
        for _ in 0..100 {
            assert_eq!(a.next_f64(), b.next_f64());
        }
        assert!(Lcg::new(1).next_f64() != Lcg::new(2).next_f64());
    }

    #[test]
    fn gate_parsing() {
        assert_eq!(
            parse_gate("latency_ms.p50=+10%"),
            Ok(("latency_ms.p50".into(), GateSpec::Percent(10.0)))
        );
        assert_eq!(
            parse_gate("connect_ms=25"),
            Ok(("connect_ms".into(), GateSpec::Absolute(25.0)))
        );
        assert!(parse_gate("nonsense").is_err());
        assert!(parse_gate("k=+10").is_err()); // percent needs the % suffix
    }

    #[test]
    fn compare_passes_and_fails() {
        let g = serde_json::json!({"latency_ms": {"p50": 10.0}});
        let good = serde_json::json!({"latency_ms": {"p50": 10.5}});
        let bad = serde_json::json!({"latency_ms": {"p50": 12.0}});
        let gates = vec![parse_gate("latency_ms.p50=+10%").unwrap()];
        assert!(compare(&g, &good, &gates).is_ok());
        assert!(compare(&g, &bad, &gates).is_err());
    }

    // NOTE (Windows): the four loopback tests below drive the REAL control
    // plane on the protocol well-known port (9943, clients always listen
    // there). They pass on Linux and on Windows *in isolation*, but on
    // Windows successive loopbacks accumulate TIME_WAIT on 9943 and the next
    // session intermittently starts with 10054/10057 mid-handshake:
    // SO_REUSEADDR lets the re-bind succeed but the stack still routes the
    // new SYN into the dying 4-tuple. The only Windows-safe teardown
    // (SO_LINGER(0)) is inherited by accepted sockets and aborts the *live*
    // session (verified on the GPU box — see the note in alvr/sockets
    // bind()), so it is not available here. These tests are therefore
    // Linux-only; the identical code path runs on the Linux tier plus the
    // golden-gate/secure/gaze jobs, while the Windows tier exists for the
    // box-specific NVENC + SteamVR driver steps. See VD_RE/20 §5.
    #[cfg_attr(
        windows,
        ignore = "fixed-port loopback is unstable on Windows; covered by the Linux tier"
    )]
    #[test]
    fn metrics_roundtrip_through_json() {
        let _guard = serial_lock();
        let s = scenario("frame_ncm").unwrap();
        let m = run_loopback(&s, 7, 8).expect("loopback run");
        assert_eq!(m.negotiation.codec, VideoCodec::Hevc); // Frame kernel: no AV1 (ADR-0008)
        assert!(m.latency.p50_ms > 0.0 && m.latency.p50_ms <= m.latency.max_ms);
        let json = serde_json::to_string(&m).expect("serialize");
        let back: RunMetrics = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.scenario, "frame_ncm");
        assert_eq!(back.negotiation.bitrate_mbps, 300);
    }

    #[cfg_attr(
        windows,
        ignore = "fixed-port loopback is unstable on Windows; covered by the Linux tier"
    )]
    #[test]
    fn full_loopback_run_with_stalls_and_events() {
        let _guard = serial_lock();
        let s = scenario("frame_cqm_churn").unwrap();
        let m = run_loopback(&s, 1234, 30).expect("loopback run");
        assert_eq!(m.iterations, 30);
        assert!(
            !m.events.is_empty(),
            "cqm churn scenario must record stall events"
        );
        assert!(m.latency.p99_ms >= m.latency.p50_ms);
    }

    #[cfg_attr(
        windows,
        ignore = "fixed-port loopback is unstable on Windows; covered by the Linux tier"
    )]
    #[test]
    fn gate_passes_for_deterministic_reruns() {
        let _guard = serial_lock();
        let s = scenario("frame_ncm").unwrap();
        let golden = run_loopback(&s, 7, 8).expect("golden");
        let rerun = run_loopback(&s, 7, 8).expect("rerun");
        gate(&rerun, &golden).expect("same-seed rerun must gate clean");
    }

    #[cfg_attr(
        windows,
        ignore = "fixed-port loopback is unstable on Windows; covered by the Linux tier"
    )]
    #[test]
    fn gate_catches_negotiation_drift() {
        let _guard = serial_lock();
        let s = scenario("frame_ncm").unwrap();
        let golden = run_loopback(&s, 7, 8).expect("golden");
        let mut run = golden.clone();
        run.negotiation.codec = VideoCodec::H264;
        assert!(gate(&run, &golden).is_err());
    }

    #[test]
    fn sanity_caps_are_profile_derived() {
        let cap = sanity_cap_ms(&profile("ncm_wired").unwrap());
        assert!(cap > 80.0 && cap < 110.0, "ncm cap {cap}");
        let cap = sanity_cap_ms(&profile("cqm_churn").unwrap());
        assert!(cap > 250.0 && cap < 350.0, "cqm cap {cap}");
    }

    #[test]
    fn delivery_stats_catch_the_one_percent_outlier() {
        let mut samples = vec![2.0; 99];
        samples.push(100.0); // the extreme example: gorgeous average, one bad frame
        let d = delivery_stats(&samples, MANDATORY_DEADLINE_MS, OPTIMAL_DEADLINE_MS);
        assert_eq!(d.missed_mandatory_pct, 1.0);
        assert!((d.max_lateness_ms - (100.0 - MANDATORY_DEADLINE_MS)).abs() < 0.01);
        assert_eq!(d.late_over_1frame_pct, 1.0);
        assert_eq!(d.best_streak, 99);
        // the 100 ms outlier also misses the optimal target (2 ms samples hit it)
        assert_eq!(d.within_optimal_pct, 99.0);

        let clean = delivery_stats(&vec![3.0; 50], MANDATORY_DEADLINE_MS, OPTIMAL_DEADLINE_MS);
        assert_eq!(clean.missed_mandatory_pct, 0.0);
        assert_eq!(clean.max_lateness_ms, 0.0);
        assert_eq!(clean.best_streak, 50);
        // 3 ms is within the 8.33 ms optimal budget
        assert_eq!(clean.within_optimal_pct, 100.0);

        // between the two budgets: mandatory-clean, optimal-imperfect
        let mid = delivery_stats(&vec![9.5; 10], MANDATORY_DEADLINE_MS, OPTIMAL_DEADLINE_MS);
        assert_eq!(mid.missed_mandatory_pct, 0.0);
        assert_eq!(mid.within_optimal_pct, 0.0);
    }

    #[test]
    fn secure_loopback_handshake_and_sealed_exchanges() {
        let m = run_secure_loopback(10, 2.0, 0.2, 7).expect("secure loopback");
        assert!(
            m.handshake_ms > 0.0 && m.handshake_ms < 2000.0,
            "handshake took {} ms",
            m.handshake_ms
        );
        assert_eq!(m.frames, 10);
        assert_eq!(m.missed_mandatory_pct, 0.0);
        assert_eq!(m.within_optimal_pct, 100.0);
    }
}

#[cfg(test)]
mod gaze_tests {
    use alvr_common::{
        AlvrFoveatedEncodingParams, Fov, Pose, ViewParams,
        glam::{Quat, UVec2, Vec3},
    };
    use std::time::{Duration, Instant};
    use x_foveation::EyeTrackedFoveation;

    /// The step response must be monotone and converge to the analytically
    /// expected shift with the DRIVER's sign convention (yaw +15° → -0.313).
    #[test]
    fn gaze_step_response_is_monotone_and_converges() {
        let view_params = [ViewParams {
            pose: Pose {
                orientation: Quat::IDENTITY,
                position: Vec3::ZERO,
            },
            fov: Fov {
                left: -55.0_f32.to_radians(),
                right: 55.0_f32.to_radians(),
                up: 45.0_f32.to_radians(),
                down: -45.0_f32.to_radians(),
            },
        }; 2];
        let params = AlvrFoveatedEncodingParams {
            encoded_view_resolution: [2048, 2048],
            view_ratio: [1.0, 1.0],
            center_size: [0.4, 0.4],
            center_shifts: [[0.0, 0.0]; 2],
            edge_ratio: [4.0, 4.0],
        };
        let mut f = EyeTrackedFoveation::new(params, UVec2::new(2048, 2048));
        f.view_params = Some(view_params);
        let poll = 1.0 / 90.0;
        let mut t = 0.0f32;
        for i in 0..27 {
            t = i as f32 * poll;
            f.update(
                Duration::from_secs_f32(t),
                Some(Quat::IDENTITY),
                Instant::now(),
            );
        }
        let mut prev = 0.0f32;
        let mut last = 0.0f32;
        for i in 1..=90 {
            t = 0.3 + i as f32 * poll;
            f.update(
                Duration::from_secs_f32(t),
                Some(Quat::from_rotation_y(15f32.to_radians())),
                Instant::now(),
            );
            let cx = f.centers(Duration::from_secs_f32(t)).unwrap()[0][0];
            assert!(
                cx <= prev + 1e-4,
                "step response must decrease monotonically (driver sign): {cx} after {prev}"
            );
            prev = cx;
            last = cx;
        }
        let expected = -0.313_f32;
        assert!(
            (last - expected).abs() < 0.01,
            "converged to {last}, expected {expected}"
        );
    }
}
