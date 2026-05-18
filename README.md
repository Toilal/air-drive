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

🚧 **MVP feature complete on the `001-minimal-sync-daemon` branch.** All six phases of
the spec are landed and tested end-to-end against a real Drive account + rclone:

- [`specs/001-minimal-sync-daemon/spec.md`](./specs/001-minimal-sync-daemon/spec.md) —
  the user stories, functional requirements, and clarification record.
- [`specs/001-minimal-sync-daemon/plan.md`](./specs/001-minimal-sync-daemon/plan.md) —
  technical context, module map, constitution check.
- [`specs/001-minimal-sync-daemon/quickstart.md`](./specs/001-minimal-sync-daemon/quickstart.md)
  — getting started from a fresh checkout.
- [`tests/e2e/README.md`](./tests/e2e/README.md) — how to set up a Google account for
  the live-Drive integration suite.

Tests: 111 unit + ~30 integration (mocked) + 3 e2e (real Drive). `cargo clippy
--all-targets --all-features -- -D warnings` clean. CI runs the mocked suite on every
push (`.github/workflows/ci.yml`); the real-Drive suite triggers on `main` + manual
dispatch (`.github/workflows/e2e.yml`).

See [`.specify/memory/constitution.md`](./.specify/memory/constitution.md) for the
project's principles, technology stack, and quality gates — this remains the source of
truth and any change goes through the constitution flow.

## Install (Linux, systemd)

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
