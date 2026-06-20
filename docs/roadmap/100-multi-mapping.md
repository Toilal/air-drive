# 100 — Multi-mapping support

- **Priority:** 🟡 medium
- **Status:** Planned
- **Issue:** [#13](https://github.com/Toilal/air-drive/issues/13)
- **Area:** cli, state

## Goal

Support more than one local↔remote folder pair per account, lifting the MVP's
singleton-mapping restriction.

## Today

The MVP writes a single `folder_mapping` row (`id = 1`) and the daemon syncs that
one pair. The schema is already keyed by `mapping_id` (see
[state schema](../dev/state-schema.md)), so the data model needs no change — the
work is in the CLI and the daemon loop.

## Approach

Let `map` register additional pairs, give each a stable identifier, and run the
watcher / poller / reconciler / dispatcher per mapping (or multiplexed across
mappings). Extend `status` to report each pair. This also unblocks persisting
`--account-label`, today accepted but dropped (see [CLI reference](../user/cli.md)).

## Acceptance

- Two or more independent folder pairs sync concurrently.
- `status` reports each mapping's state.
- Restart resumes every mapping from persisted state.
