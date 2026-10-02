# ADR-0012 — Upstream ALVR is abandoned

- **Status:** Accepted
- **Date:** 2026-10-02
- **Deciders:** maintainer
- **Supersedes in practice:** CHARTER value #4 ("Independence without isolation"), `INTEROP.md`, `PATCHES.md`'s framing, and the "check ALVR first" half of the handoff's §8c. *(CHARTER.md itself is deliberately not edited — amending the charter is a separate act; see Consequences.)*

## Context

GemLink was forked from ALVR at a pinned HEAD (`9f118394`, ADR-0002) with a
stated hedge: keep the wire protocol compatible "where cheap", so the door back
never closes, and prefer extending upstream mechanisms over adding parallel ones.
That hedge priced a rebase that was expected to keep happening.

Three things made the hedge stop paying:

1. **The fork has already left.** The Steam Frame controller path, the monotonic
   frame index, the Steam Frame profile vocabulary and the settings-schema
   additions are all deviations in shared crates — the last session had to admit
   this in `PATCHES.md`, which had been claiming "none yet". A rebase budget that
   size is not a hedge, it is a second project.
2. **Nothing is coming back.** Upstream has no releases to rebase onto, no
   maintainer responding, and no direction to converge with. "Wire-compatible
   with upstream" now means "wire-compatible with a frozen snapshot", which is a
   self-imposed constraint with no counterparty.
3. **It was actively harmful to reasoning.** The handoff's §8c told the project to look
   for *the inherited answer* before designing anything. Applied honestly that rule would
   have found the grey-frame root cause immediately — the bounded channel and `try_send`
   drop that produced the 41 % grey rate is inherited ALVR code, sitting in
   `server_core/src/lib.rs`. Instead the rule was run as "find the design-intent comment",
   a comment was quoted as settled, and three wrong fixes shipped on the strength of it. The
   rule is not wrong about *reading* inherited code; it is wrong about **treating it as an
   authority**, which is what it invites. ALVR is a reference implementation: it is where
   the answer was hiding *and* where the bug was.

## Options considered

1. **Keep the hedge: stay rebase-friendly, stay wire-compatible.** — Pros: a
   possible future merge; continuity for ALVR users. Cons: prices every change
   against a moving target that no longer moves; keeps `sockets`/`server_core`/
   `session` shaping decisions we would not otherwise make; the budget is already
   spent.
2. **Fork cleanly into the `x-*` crates and freeze the ALVR tree.** — Pros:
   keeps a known-good baseline and the inherited test corpus. Cons: two mental
   models forever, and "which side does this belong on" becomes a per-change
   debate with no owner.
3. **CHOSEN — treat upstream as abandoned: reference, not authority.**
   No rebase budget, no wire-compat obligation, no inherited-mechanism
   preference. The ALVR code stays, is still read, and is still allowed to be
   the *right* answer when it happens to be — but it stops being evidence.

## Decision

1. **No precedent rule.** "Upstream ALVR does X" is not an argument for X. It is
   a source of ideas and a source of bugs, in that order of usefulness.
2. **No rebase budget.** `PATCHES.md` is reclassified: it is a *provenance
   record* of what the fork changed, kept so we can tell our code from inherited
   code during debugging. `rustfmt`/`clippy` cleanliness of shared crates
   continues, because those cost nothing; structural symmetry with upstream is
   abandoned as a goal.
3. **No wire-compatibility obligation.** `INTEROP.md` becomes historical. GemLink
   speaks GemLink's protocol. The `alvr_*` packets/sockets crates may be replaced
   wholesale when the transport work reaches them.
4. **VD becomes the only external spec that matters.** The pre-solve check
   (handoff §8c) is narrowed to Virtual Desktop, and only where VD's behaviour is
   *recoverable* — a large part of its link layer is not (see
   `VD_RE/24-vd-link-qos.md` §7), and "unrecoverable" must not be filled in with
   a guess dressed as evidence.
5. **The inherited test corpus is retained.** `x-bench`'s scenarios, the golden
   metrics and the upstream crates that drive authentic framing are still
   valuable as a *reference implementation* — conformance by construction — and
   are not deleted on principle.

## Consequences

**Easier.** Design space opens where it was previously rationed: the media plane
(ADR-0011's client half is no longer "extend `avoid_video_glitching`", it is
"design the media plane we actually want"), the transport, and the settings
schema, which no longer has to look like ALVR's tree. Fewer two-mind-model
arguments.

**Harder.** We lose an inherited correctness oracle. ALVR's code encoded years of
tuning against real headsets and real drivers, and "our version" is now
unverified wherever we diverge — which raises the value of the bench and of the
field reports in `COMPAT.md`, and means the gates in `ROADMAP.md` are now doing
load-bearing work a rebase used to do. Licensing is unaffected (MIT, NOTICE
intact — ADR-0001).

**Standing debt.** Three shared crates are now *ours* in all but name
(`sockets`, `packets`, `server_core`'s connection path). Owning them piecemeal is
the worst of both worlds; the transport work should either adopt them properly or
replace them, and that choice belongs in the transport ADR rather than here.

**Not done by this ADR.** `CHARTER.md` value #4 ("Independence without isolation —
wire-protocol compatibility with upstream is maintained where cheap, so the door
back never closes") states the superseded position as a *value*. A charter
amendment is the maintainer's call and is left to them; until it happens, this ADR
and the charter disagree, and this ADR is the operative one.

**Revisit if** the ALVR project returns to active maintenance with a release we
would want to merge — at which point the price of option 2 (a clean re-fork) is
worth re-estimating, not the price of a rebase.
