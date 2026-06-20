# air-drive

Open source Google Drive sync client for Linux (then macOS and Windows), with
**bidirectional event-driven synchronization on both sides** — inotify locally,
`changes.list` + `pageToken` remotely.

## Why

Google does not ship an official Google Drive client for Linux. The only confortable
third-party alternative (Insync) is paid, and existing OSS tools (`rclone bisync`,
`google-drive-ocamlfuse`, GNOME Online Accounts) either rely on periodic polling or
are not real sync engines.

air-drive aims to fill that gap with a small Rust daemon that reacts to events on
both sides, embeds `rclone` as the underlying sync engine, and exposes a lightweight
desktop UI via Tauri.

## Status

🚧 **MVP shipped.** Bidirectional event-driven sync, single Drive account, one
mapped folder pair, integration-tested against a mocked Drive API and end-to-end
against a real Drive account + rclone. See
[`tests/e2e/README.md`](./tests/e2e/README.md) for how to set up a Google account
for the live-Drive integration suite.

Tests: 111 unit + ~30 integration (mocked) + 3 e2e (real Drive). `cargo clippy
--all-targets --all-features -- -D warnings` clean. CI runs the mocked suite on every
push (`.github/workflows/ci.yml`); the real-Drive suite triggers on `main` + manual
dispatch (`.github/workflows/e2e.yml`).

See [`CLAUDE.md`](./CLAUDE.md) for the project's principles, technology stack, and
quality gates.

## Documentation

Full documentation lives under [`docs/`](./docs/), indexed by
[`docs/README.md`](./docs/README.md):

- **User guides** — [installation](./docs/guide/installation.md),
  [CLI reference](./docs/guide/cli.md),
  [configuration](./docs/guide/configuration.md),
  [OAuth setup](./docs/guide/oauth-setup.md).
- **Internals** — [architecture](./docs/internals/architecture.md),
  [sync model](./docs/internals/sync-model.md),
  [state schema](./docs/internals/state-schema.md),
  [development](./docs/internals/development.md).

Contributors should also read [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Install (Linux, systemd)

### One-liner

```sh
curl -fsSL https://raw.githubusercontent.com/Toilal/air-drive/main/install.sh | bash
```

The script picks the right target triple for your kernel/arch (defaults to the
fully-static `musl` build), pulls the latest release tarball from the GitHub
Release page, verifies its SHA-256 against the published `.sha256` sibling, and
drops `air-drive` into `~/.local/bin/`. Pass `--systemd` to also enable the
systemd user unit:

```sh
curl -fsSL https://raw.githubusercontent.com/Toilal/air-drive/main/install.sh \
    | bash -s -- --systemd
```

### From source

After building (`cargo build --release` → `target/release/air-drive`), drop the binary
on your `$PATH` and let `setup --install-service` drop the systemd user unit:

```sh
install -m 0755 target/release/air-drive ~/.local/bin/
air-drive setup --install-service
```

That copies [`assets/systemd/air-drive.service`](./assets/systemd/air-drive.service)
to `~/.config/systemd/user/air-drive.service` and runs
`systemctl --user enable --now air-drive.service`. Logs land in journald
(`journalctl --user -u air-drive -f`).

Then link the account + map a folder once:

```sh
air-drive link
air-drive map ~/Drive 'path:My Drive/Sync'
air-drive status         # confirms state, mapping, pending counters
```

## OAuth scope

air-drive requests the full `https://www.googleapis.com/auth/drive` scope on
the consent screen — wording on Google's prompt is roughly *"See, edit, create,
and delete all of your Google Drive files"*. The narrower `drive.file` scope
only exposes files the daemon itself created, which makes it impossible to
sync an already-populated Drive folder (the original blocker reported in
issue #4).

Google classifies `drive` as a sensitive scope. OAuth clients in `Testing`
mode work fine but cap refresh tokens at 7 days, so the daemon will prompt for
re-consent every week. Moving the client to `Production` requires Google's
OAuth verification review (security assessment, homepage + privacy-policy
URLs, possibly a demo video). See [`CLAUDE.md`](./CLAUDE.md) §V for the
constitution-level rationale.

## Stack

- **Language**: Rust (stable)
- **Async runtime**: tokio
- **Local watcher**: `notify`
- **HTTP / OAuth**: `reqwest` + `yup-oauth2`
- **Persistence**: SQLite via `rusqlite`
- **UI**: Tauri v2
- **Sync engine**: `rclone` embedded as a subprocess (behind a `SyncEngine` trait —
  a native Rust engine is the long-term goal)

## License

[Apache License 2.0](./LICENSE). See [`NOTICE`](./NOTICE) for attribution.

When the v1.0 bundle ships the `rclone` binary, third-party licenses (MIT for rclone,
others for transitive deps) will be listed in `THIRD_PARTY_LICENSES`.
