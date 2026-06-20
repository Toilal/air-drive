# air-drive documentation

Documentation for **air-drive**, an open-source, event-driven Google Drive sync
client for Linux. This index links every document under `docs/`. For the
project's principles, technology stack, and quality gates, see
[`../CLAUDE.md`](../CLAUDE.md).

## User guides

For people running air-drive on their machine.

- [Installation](guide/installation.md) — install the binary, set up the
  systemd user service, first-run wizard.
- [CLI reference](guide/cli.md) — every command, flag, and exit code.
- [Configuration](guide/configuration.md) — `config.toml` keys, on-disk paths,
  ignore patterns, auto-migration.
- [OAuth setup](guide/oauth-setup.md) — why air-drive needs the full `drive`
  scope and how to use your own Google Cloud OAuth client.

## Project

- [Roadmap](roadmap.md) — what's planned after the MVP, linked to the issue
  tracker.

## Internals

For contributors and anyone curious about how the daemon works.

- [Architecture](internals/architecture.md) — daemon orchestration, module
  layout, the four concurrent loops.
- [Sync model](internals/sync-model.md) — event-driven sync on both sides,
  reconciliation, conflicts, echo suppression.
- [State schema](internals/state-schema.md) — the SQLite tables, versioned
  migrations, what lives where.
- [Development](internals/development.md) — build, test tiers, quality gates
  (points to [`../CONTRIBUTING.md`](../CONTRIBUTING.md)).

## See also

- [`../README.md`](../README.md) — project overview and quick start.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — contribution workflow and
  conventions.
- [`../tests/e2e/README.md`](../tests/e2e/README.md) — setting up a Google
  account for the live-Drive test suite.
