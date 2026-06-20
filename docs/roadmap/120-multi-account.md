# 120 — Multi-account

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** cli, drive, state

## Goal

Link N Google Drive accounts in a single daemon, each with its own mapping(s).

## Today

The MVP persists a single `account` row (`id = 1`). The schema is multi-account
by design — `folder_mapping.account_id` and everything downstream key off it (see
[state schema](../dev/state-schema.md)) — but `link` / `unlink` / `status` and
the token store assume one account.

## Approach

Let `link` add additional accounts (distinct `tokens.json` per account), associate
mappings with an account, and surface per-account state in `status`. Builds on
[100 — multi-mapping](100-multi-mapping.md). The `--account-label` flag becomes
meaningful here.

## Acceptance

- Two accounts can be linked, each driving its own mapping(s).
- `unlink` removes one account without touching the others.
