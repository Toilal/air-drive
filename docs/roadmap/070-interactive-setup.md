# 070 — Interactive setup wizard

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** cli

## Goal

`air-drive setup` walks a first-time user through `link → map → start
--initial-sync` interactively, so onboarding is a single guided command.

## Today

`src/cli/setup.rs` reports that interactive mode "is not yet implemented in this
MVP"; users drive each subcommand individually. The `--install-service` /
`--uninstall-service` flags of `setup` already work (see
[CLI reference](../user/cli.md) and [installation](../user/installation.md)).

## Approach

Implement the guided flow: trigger the OAuth `link`, prompt for the local folder
and remote target (honouring `auto_create_root` / `auto_create_remote_root`), then
kick off the initial sync. Degrade gracefully on non-interactive stdin.

## Acceptance

- `air-drive setup` with no flags completes a full first-time configuration
  interactively.
- Non-interactive stdin produces a clear, actionable error rather than hanging.
