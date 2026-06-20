# 010 — rclone access-token refresh

- **Priority:** 🔴 critical
- **Status:** Implemented — pending e2e verification against real Drive
- **Issue:** [#5](https://github.com/Toilal/air-drive/issues/5)
- **Area:** sync, engine, drive/auth

## Goal

A single rclone operation that outlives the OAuth access-token lifetime
(~1 h) — a large upload/download — must complete instead of failing with
HTTP 401.

## Today

`RcloneEngine::base_command()` calls `TokenProvider::token()` on **every**
invocation, and `YupOAuthProvider` refreshes through yup-oauth2 internally, so
each *new* rclone process starts with a fresh access token. The bug is therefore
scoped to **one operation that runs past the token's real expiry**:
`format_token_json()` hands rclone `expiry: 2099-01-01`, so rclone believes the
token is eternally fresh and never refreshes mid-transfer → `401`.

rclone can't refresh on its own today because we give it neither a
`refresh_token` nor a `client_secret` (only an optional `client_id`).

## Approach

Give rclone a credential it can refresh itself, so it self-refreshes during a
long transfer:

1. **Expose richer credentials from `TokenProvider`.** Add a trait method
   (default impl returns access-token only, so `StaticToken` and tests are
   unaffected) returning `{ access_token, refresh_token, expiry_rfc3339 }`.
   `YupOAuthProvider` overrides it:
   - access token + real expiry come from yup-oauth2's `AccessToken`
     (`token()` / `expiration_time()`), formatted RFC 3339;
   - the `refresh_token` is read from `tokens.json` — a JSON array of
     `{ scopes, token: { access_token, refresh_token, expires_at, id_token } }`;
     parse as a `serde_json::Value`, pick the entry matching `DRIVE_SCOPES`, and
     take `token.refresh_token`. Parsing the value (not the typed struct) avoids
     coupling to `time`'s `expires_at` serialization.
2. **Feed rclone a full token + client creds.** `format_token_json()` emits
   `access_token` + `token_type` + the **real** `expiry` + `refresh_token` (when
   present). `base_command()` also sets `RCLONE_CONFIG_AIRDRIVE_CLIENT_ID` and
   `RCLONE_CONFIG_AIRDRIVE_CLIENT_SECRET` from `[oauth]`. `--config /dev/null`
   stays: rclone refreshes in-memory and silently discards the write-back, which
   is fine — yup-oauth2 owns the canonical refresh token.
3. **Thread `client_secret`** into `RcloneEngine` (it already holds
   `client_id`); pass it from `build_engine` in `cli/runtime.rs`.

### Scope note (depends on [#180](180-oauth-production.md))

Google refreshes a *desktop* token using `client_id` **and** `client_secret`.
The embedded client is still a placeholder with no secret, so self-refresh works
today only for users who set their own `[oauth].client_id` + `client_secret`; the
embedded client inherits the fix once #180 ships real credentials. When a
`refresh_token` is available but client creds are not, log a clear warning rather
than failing.

## Acceptance

- [x] `format_token_json` includes `refresh_token` + real `expiry` when supplied;
  unit-tested.
- [x] `tokens.json` refresh-token extraction is unit-tested against a fixture.
- [x] No regression in the mocked engine integration suite; `RcloneEngine::new`
  signature change threaded through all call sites.
- [ ] **A transfer that outlives the access-token lifetime completes without
  `401`** — verifiable only by the e2e suite against real Drive (large file +
  user-supplied `[oauth]` creds). This is what's left before the entry can be
  deleted.

## Implementation notes

Landed in `fix(engine): let rclone self-refresh the access token`. `TokenProvider`
gained `rclone_token()` (default returns access-only; `YupOAuthProvider` supplies
refresh token from `tokens.json` + real RFC 3339 expiry). `RcloneEngine` now
passes `client_secret` and the full token JSON. `--config /dev/null` kept. Remaining
caveat: embedded-client users can't self-refresh until [#180](180-oauth-production.md)
ships real credentials.
