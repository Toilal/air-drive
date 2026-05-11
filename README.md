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

🚧 Early stage — design phase. See `.specify/memory/constitution.md` for the project's
principles, technology stack, and quality gates (this file is the source of truth).

## Stack (planned)

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
