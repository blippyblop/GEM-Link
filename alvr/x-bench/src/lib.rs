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

pub const SCHEMA_VERSION: u32 = 1;
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

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: &'static str,
    pub profile: ImpairmentProfile,
    pub client: ClientCapabilities,
    pub server: ServerCapabilities,
    pub gating: bool,
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
        latency: latency_stats(samples),
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

    #[test]
    fn metrics_roundtrip_through_json() {
        let _guard = serial_lock();
        let s = scenario("frame_ncm").unwrap();
        let m = run_loopback(&s, 7, 8).expect("loopback run");
        assert_eq!(m.negotiation.codec, VideoCodec::Av1);
        assert!(m.latency.p50_ms > 0.0 && m.latency.p50_ms <= m.latency.max_ms);
        let json = serde_json::to_string(&m).expect("serialize");
        let back: RunMetrics = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.scenario, "frame_ncm");
        assert_eq!(back.negotiation.bitrate_mbps, 300);
    }

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
}
