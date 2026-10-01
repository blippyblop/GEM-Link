# BENCH — what we measure, how, and what the numbers mean

*Charter value #3: **measured, not vibes**. Every feature lands with a scenario here
and a gate in CI. The score itself is defined in [../ROADMAP.md](../ROADMAP.md#the-score).*

`x-bench` is the measurement harness. It drives the **real** control-plane wire
protocol through upstream's own socket and packet crates — the fake headset speaks
authentic framing because it *is* the authentic framing (conformance by
construction) — under deterministic link-impairment profiles.

```
bench run      <scenario> [--seed N] [--iterations N] [--out DIR]
bench compare  <golden.json> <candidate.json> [--gate key=+X% | key=ABS]...
bench gate     --run <metrics.json> [--goldens DIR]
bench secure   [--iterations N]      # Noise-XX AEAD loopback: handshake + sealed RTT
bench gaze                           # eye-gaze → foveation-center driver math
bench nvenc    [--bit10] [--seconds N]   # real NVENC delivery gates (Windows GPU tier)
bench scenarios
```

## The deadlines (where the numbers come from)

`MANDATORY_DEADLINE_MS = 1000/90 = 11.11 ms` · `OPTIMAL_DEADLINE_MS = 1000/120 = 8.33 ms`.

90 Hz is the **gate**; 120 Hz is the **published target**, tracked not enforced. The
primary metric is never the mean — it is *how many frames blow a deadline and by how
much*. A beautiful mean with 1 % late frames is a broken experience.

## Metrics schema (`metrics.json`, schema v4)

| Field | Meaning |
|---|---|
| `negotiation.*` | codec, fps, bitrate, foveation mode, link class, encryption, per-eye size the session settled on (`per_eye_width`/`height` — **the client's** choice) |
| `connect_ms` | control-plane connect time |
| `latency.p50/p95/p99/mean/max_ms` | per-frame latency under the modelled profile — **loopback, not glass-to-glass** |
| `delivery.mandatory_ms` / `optimal_ms` | the two budgets above |
| `delivery.missed_mandatory_pct` | **% of frames late for 90 Hz — the gate is 0.0** |
| `delivery.within_optimal_pct` | % inside the 120 Hz budget — the published target |
| `delivery.max_lateness_ms` | worst overshoot past the mandatory deadline (0 if none) |
| `delivery.late_over_1frame_pct` | % late by more than a whole extra frame |
| `delivery.best_streak` | longest consecutive run inside the mandatory deadline |
| `profile.*` | the impairment model that produced the run |

## Scenarios and profiles

| Scenario | Profile | Gating | Notes |
|---|---|---|---|
| `frame_ncm` | `ncm_wired` | ✅ | USB-C NCM — the lowest-latency profile |
| `frame_wifi7_160` | `wifi7_160_clean` | ✅ | Wi-Fi 7 / 6 GHz / 160 MHz, dedicated airtime |
| `frame_wifi7_regrace` | `wifi7_regrace` | — | 6 GHz regulatory-race TX-cap failure mode |
| `frame_cqm_churn` | `cqm_churn` | — | roaming/CQM churn |
| `quest_pro_wifi6` | `wifi6_lan` | — | Tier-2 device, informational |
| `quest3_wifi6` | `wifi6_lan` | — | Tier-2 device, informational |

Profiles are modelled on **measured** Frame link behaviour (ADR-0003), not generic
Wi-Fi defaults. Only Steam Frame scenarios gate releases (ADR-0004).

## Gate rules (`bench gate`)

A run is compared against a golden in `bench/goldens/`. Gates checked:

- `schema_version`, `seed`, `iterations`, `profile`, `negotiation` — must match the golden
- `event_sequence` — kind + detail sequence must match
- `latency_p99_sanity` — p99 under a profile-derived cap
- `missed_deadlines` — **gating scenarios only**: `missed_mandatory_pct == 0.0`

CI (`gemlink-linux.yml`) runs all six scenarios at seed 42 / 50 iterations and gates
each against its golden, then runs the `secure` and `gaze` tiers.

## What the bench does **not** measure

Being explicit here is the point — a harness that quietly overstates its coverage is
worse than no harness.

- **Glass-to-glass latency.** There is no shared clock between the PC and the client,
  so the rig reports cadence deviation and per-stage server statistics and nothing
  else. `latency.*` is loopback latency under a modelled profile. DoD #3's field bar
  (≤ Steam Link/VR Link on identical hardware) is **blocked on hardware**.
- **Client coverage.** The runtime-less rig is transport/protocol coverage. Per
  ADR-0009, a harness that omits the runtime measures compatibility with something the
  product will never meet; it is labelled as such, never treated as client coverage.
- **Real decode or presentation.** `x-framesim` has a **null decoder**; decode and
  presentation are unexercised until M3.

## Adding a measurement

1. New feature ⇒ new `Scenario` (or a new gate on an existing one) and, for gating
   scenarios, a golden.
2. Determinism only: seeded RNG, no wall-clock assertions (`CONTRIBUTING.md` rule 5).
3. `bench run <scenario> --seed 42 --iterations 50` → commit the golden.
4. Wire it into `gemlink-linux.yml` (Linux) or the Windows GPU tier (NVENC).

## Planned (not yet built)

- **Glass-to-glass harness** — the DoD #3 method. Needs a Frame: a shared clock
  (or a photodiode/light-to-photon rig) so PC-side and client-side timestamps are
  comparable. This is the project's score and its absence is the top measurement risk.
- **Real-source runs** — gate on a real game + real SteamVR compositor instead of the
  synthetic source (see `VD_RE/23-benchmarking-ideas.md`).
- **Tail-latency diagnosis tooling** — per-stage ring buffers, decoder-config capture,
  the deck of options shortlisted in `VD_RE/23-benchmarking-ideas.md`.
- **CI coverage guard** for `client_core`, `x-framesim`, `x-pcsim` (currently unbuilt).
