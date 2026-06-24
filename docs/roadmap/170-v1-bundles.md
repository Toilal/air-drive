# 170 — v1.0 self-contained bundles

- **Priority:** —
- **Status:** Planned
- **Issue:** —
- **Area:** distribution, licensing

## Goal

Ship v1.0 as self-contained, installable bundles per platform: a Linux AppImage,
a macOS `.app`, and a Windows installer — each embedding `rclone`.

## Today

Distribution is the `install.sh` one-liner that downloads the release binary; the
`rclone` binary is fetched separately on first run (auto-download with SHA-256
verification, see [`../user/installation.md`](../user/installation.md)). No bundles
yet.

## Approach

Package each platform's bundle with the daemon, the UI
([140 — desktop UI](140-desktop-ui.md)), and the embedded `rclone`. Per
[`../../CLAUDE.md`](../../CLAUDE.md) §III, any bundle redistributing `rclone` MUST
include its MIT copyright notice and license text, and all third-party licenses
MUST be listed in a `THIRD_PARTY_LICENSES` file.

On Linux, bundle the **native Nautilus extension** here (carried over from
[150 — desktop shell integration](150-desktop-shell-integration.md)): a C `.so`
built against `libnautilus-extension-4`, installed into the *system* extensions
dir by the package, dropping the `python3-nautilus` runtime dependency the
user-dir Python extension needs. The Python extension stays the non-packaged
fallback.

## Acceptance

- Installable AppImage / `.app` / Windows installer that run with no manual
  dependency setup.
- `THIRD_PARTY_LICENSES` present and complete; rclone's MIT notice included.
