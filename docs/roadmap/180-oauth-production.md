# 180 — OAuth Production verification

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** drive, distribution

## Goal

Move the OAuth client from `Testing` to `Production` so users stop having to
re-consent every 7 days.

## Today

The broad `https://www.googleapis.com/auth/drive` scope is sensitive. In `Testing`
mode Google caps refresh-token lifetime at 7 days, so the daemon prompts for
re-consent roughly weekly (see [OAuth setup](../user/oauth-setup.md) and
[`../../CLAUDE.md`](../../CLAUDE.md) §V). The auth flow also carries an MVP
placeholder (`src/drive/auth.rs`) flagged to be replaced before the first public
release.

## Approach

Replace the auth MVP placeholder, then complete Google's OAuth verification:
security assessment, a published homepage and privacy-policy URL, and likely a
demo video. Coordinate with the [v1.0 bundles](170-v1-bundles.md) release.

## Acceptance

- The published OAuth client is in `Production`.
- A linked account keeps its refresh token well beyond 7 days.
- The auth placeholder is gone.
