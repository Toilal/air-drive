# 050 — rclone auto-download

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** engine, distribution

## Goal

On first run, fetch a verified `rclone` binary automatically so users don't have
to install it themselves — the MVP distribution promise in
[`../../CLAUDE.md`](../../CLAUDE.md) §VI.

## Today

`src/engine/rclone_path.rs` errors with "automatic rclone download is not yet
implemented"; the daemon requires a pre-existing `rclone` found via
`[rclone].path`, `$PATH`, or the cache. `--no-download-rclone` is already wired as
the opt-out flag (see [CLI reference](../user/cli.md)).

## Approach

Post-install download from `downloads.rclone.org` with **SHA-256 verification**,
cached at `~/.cache/air-drive/bin/rclone`. Pick the binary matching the host
target triple. Respect `--no-download-rclone` (fail fast when set and no binary
is found) and `[rclone].path` (skip the download entirely).

## Acceptance

- A clean machine with no `rclone` can `air-drive start` and get a verified
  binary cached.
- Corrupt/again-mismatched downloads are rejected on the SHA-256 check.
- `--no-download-rclone` still fails fast with an actionable error.
