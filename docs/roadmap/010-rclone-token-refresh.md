# 010 — rclone access-token refresh

- **Priority:** 🔴 critical
- **Status:** Planned
- **Issue:** [#5](https://github.com/Toilal/air-drive/issues/5)
- **Area:** sync, engine

## Goal

Long-running rclone operations must survive an access-token expiry instead of
failing with HTTP 401.

## Today

The daemon hands rclone a token with an expiry set far in the future
(`src/engine/rclone.rs`), so rclone treats it as fresh and never refreshes it.
Once the real Google access token expires mid-transfer, the operation fails with
`401 Unauthorized`. Short operations usually finish inside the token's real
lifetime and hide the bug; large uploads/downloads hit it.

## Approach

Give rclone a credential it can refresh itself (a proper refresh-token config),
or refresh proactively in `drive::auth` and feed rclone a still-valid token
before each operation. Decide which fits the `SyncEngine` boundary best (see
[architecture](../dev/architecture.md)) — the daemon already owns the OAuth
refresh flow.

## Acceptance

- A transfer that outlives the access-token lifetime completes without 401.
- No regression in the mocked engine integration suite.
