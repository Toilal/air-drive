# 190 — Native Rust engine

- **Priority:** —
- **Status:** Planned (long-term)
- **Issue:** —
- **Area:** engine

## Goal

A from-scratch Rust sync engine (`NativeEngine`) that replaces the `rclone`
subprocess, removing the external binary dependency.

## Today

The only `SyncEngine` implementation is `RcloneEngine`, driving the `rclone`
binary via `tokio::process::Command`. Application code already depends only on the
`SyncEngine` trait (see [architecture](../dev/architecture.md) and
[`../../CLAUDE.md`](../../CLAUDE.md) §IV), so a native engine is substitutable
without touching the rest of the daemon.

## Approach

Reimplement, incrementally, the edge cases `rclone` already solves — native Google
Docs, shortcuts, shared folders, throttling, error recovery, renames — behind the
existing trait. This is the long-term goal, not a near-term one; `rclone` stays
the default until the native engine reaches parity.

## Acceptance

- `NativeEngine` passes the same engine integration suite as `RcloneEngine`.
- It can be selected without changes outside the `engine` module.
