# GOVERNANCE

## Roles
- **Maintainers**: commit access to `main`, release authority, RFC votes. Start: 1 (you). Recruit co-maintainers by milestone M2.
- **Contributors**: anyone with a merged PR (DCO-signed).
- **Users**: everyone else — their bug reports and compat data are first-class input.

## Decision making
- **Routine**: maintainer merges; any contributor may object within 7 days.
- **Architecture / identity / license**: public **RFC** (GitHub Discussion, 14-day window) resolved by maintainers; every resolved decision gets an **ADR** in `docs/adr/` — including decisions made by a solo maintainer. The ADR log is the project's memory and its legitimacy.

## Upstream relations
- Small fixes and bench improvements may be PR'd upstream (alvr-org) on a light schedule.
- Identity features are never upstreamed uninvited; wire-protocol compatibility is maintained so cooperation stays possible.

## Conduct
Adopt the Contributor Covenant (or equivalent). Two-maintainer rule for moderation actions once the team grows.

## Changes to this document
Via RFC + ADR.
