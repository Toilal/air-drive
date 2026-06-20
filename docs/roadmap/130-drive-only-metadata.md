# 130 — Drive-only metadata

- **Priority:** ⚪ low
- **Status:** Planned
- **Issue:** [#9](https://github.com/Toilal/air-drive/issues/9)
- **Area:** state, docs

## Goal

Decide and document how air-drive treats Drive-only metadata that has no local
filesystem equivalent: permissions/sharing, file revisions, and comments.

## Today

There is no defined policy. These are simply not synced, and that choice isn't
written down anywhere.

## Approach

This is primarily a **decision + documentation** task: settle whether each of
permissions, revisions, and comments is out of scope, preserved (not clobbered),
or surfaced — and record the rationale. Implement only what the decision
requires.

## Acceptance

- A documented policy for permissions, revisions, and comments.
- Behaviour matches the documented policy (e.g. nothing silently clobbers remote
  sharing).
