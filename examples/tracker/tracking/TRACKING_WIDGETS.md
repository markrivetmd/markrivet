---
status: active
entity: widgets
owner: owner
---

# Widgets

> State for the widget rollout. The design lives elsewhere; this file keeps
> the queue and the log.

## Action

- [ ] **Ship the widget. (2026-09-01)** The release branch is cut; the
      changelog still needs the migration note.
      **Gate**: `make release-check` passes.

- [ ] `agent` **Refresh the fixtures. (2026-09-03)** Regenerate from the
      seeded corpus.
      **Gate**: the fixture diff is empty.

- [x] `human:sam` **Confirm the pilot customer. (2026-08-28)**
      **Gate**: reply on the thread.

## Notes

Background a session needs, not a queue. Nothing addresses these items, so a
default stamp leaves them alone; `select: {"kind": "document"}` stamps them.

- The rollout is gated on the migration note, not on the build.
- `widget-cli release --to <channel>` is the only supported publish path.

## Log

Newest first, one entry per event.

### 2026-09-03: fixtures drifted

The seeded corpus changed shape; the fixtures need regenerating.

### 2026-08-28: pilot confirmed

Sam confirmed the pilot customer.
