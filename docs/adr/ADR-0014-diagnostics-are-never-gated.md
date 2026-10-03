# ADR-0014 — Diagnostics are never gated by a user setting

- **Status:** Accepted
- **Date:** 2026-10-03
- **Deciders:** maintainer

## Context

Over two sessions, four defects were made invisible by configuration rather than by being
absent. In every case the code was working as written, nothing logged an error, and the
information needed to find the problem had been suppressed before anyone could read it.

1. **`env_logger` with no `RUST_LOG` is Error-only.** Every `warn!` the client emitted was
   discarded by default. A run produced `0 × Network dropped video packet` — a number that
   was treated as a measurement for a session and was in fact *never printed*. ("Void — do
   not inherit", `50-grey-frame-experiments.md`.)
2. **`extra.logging.debug_groups` filters by group, independent of level.** The setting
   applies to `dbg_*` targets regardless of severity, so an entire C++ driver path — every
   `Present` — was dropped **even in a `traced` build**. A traced build proved necessary and
   not sufficient, at a cost of hours.
3. **`client_log_report_level` defaulted to `Error`.** The client's log mirror to the server
   therefore carried no warnings at all by default. Every interesting client failure this
   project has had was announced at `warn!`.
4. **`avoid_video_glitching` gated the recovery itself.** Stored `false` in the box's
   `session.json`, it disabled the keyframe request, so the client held every frame and never
   asked for the one that would release it: 5,100 frames received, 0 decoded, a black
   screen. The setting was user-facing; the consequence was invisible.

The pattern is not "logging is too quiet". It is that **a preference was allowed to decide
whether failure is reported**, and the two are not the same kind of thing. A user may
reasonably choose how much *success* they want described. No user, and no default, should be
able to choose that a fault goes unmentioned — least of all to the people debugging it.

There is a second-order cost, which is why this is an ADR and not a three-line fix: once a
project has been burned this way, every absence of evidence becomes ambiguous. A clean log
stops meaning "nothing went wrong" and starts meaning "possibly the log was filtered", and
the only way to tell is to re-run with different settings. That tax is paid on every
investigation from then on.

## Decision

1. **Warnings and errors are never filtered.** A user setting may choose the verbosity of
   `Info`, `Debug` and `Trace`. It may not suppress `Warn` or `Error`, by level or by group.
2. **Group filters apply only to debug output.** A group is a topic for *extra* detail, not a
   switch for whether a fault is reported.
3. **Defaults are chosen to show faults.** The client's default log filter is `warn` and the
   client-to-server report level defaults to `Warning`. A default that hides a fault is a bug
   in the default, not a conservative choice.
4. **A diagnostic that describes a fault is not a debug feature.** Counters and summaries — the
   socket discard count, the send-queue depth, the stall ladder's escalations, the media
   plane's held/abandoned/repaired counts — are part of the behaviour, not an optional extra,
   and are reported at a severity that survives every filter.
5. **Behaviour must not be gated by a setting whose consequence is invisible.** A setting may
   change *how* the client responds to a fault; it may not silently remove the response. Where
   a setting's only remaining effect is to disable an invariant, it is deprecated rather than
   left as a switch that appears to do something.

## Consequences

- The client's `send_log` now short-circuits both filters for `Error` and `Warning`, and the
  client's non-Android logger defaults to `warn`. This matches what the *server* already did —
  `meta.level() <= Info` always passes there, and only `Debug`/`Trace` consult the groups — so
  the change makes the two ends consistent rather than inventing a policy.
- `client_log_report_level` defaults to `Warning`.
- `avoid_video_glitching` no longer gates anything and its help text says so. It is retained
  only so existing session files load; removing the field is a separate, schema-affecting
  change.
- Warnings can be noisier than before. That is the intended trade and the correct direction:
  the cost of an unread warning is a log line, and the cost of a suppressed one has been two
  days of chasing a network that was innocent.
- **Reviewed by measurement, not by reading.** The claim to check is that a client with a
  configured verbosity still reports a hold, a discard and a stall. `clippy` and unit tests
  can assert the filter logic; only a run can show the lines.

## Not decided here

Whether the client should report faults *more loudly* than a log line — an on-screen
indicator, a counter surfaced to the PC, a non-fatal but visible state — is left open. The
in-VR link state (VD renders one, we have the data and no renderer) is the natural home, and
it is already on the roadmap.
