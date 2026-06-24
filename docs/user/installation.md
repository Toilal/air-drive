# Installation

air-drive ships as a single self-contained binary for Linux. The MVP targets
Linux x86_64 and aarch64 (systemd user service). macOS and Windows come later.

## Requirements

- A Linux system with a user systemd instance (for the background service;
  optional if you run the daemon in the foreground).
- `rclone` v1.65+ — the daemon **auto-downloads** a verified copy on first run
  to `~/.cache/air-drive/bin/rclone` unless you pass `--no-download-rclone` or
  point `[rclone].path` at an existing binary (see
  [configuration](configuration.md)).

## Install the binary

### One-liner

```sh
curl -fsSL https://raw.githubusercontent.com/Toilal/air-drive/main/install.sh | bash
```

The script picks the right target triple for your kernel/arch (defaults to the
fully-static `musl` build), pulls the latest release tarball from the GitHub
Release page, verifies its SHA-256 against the published `.sha256` sibling, and
drops `air-drive` into `~/.local/bin/`.

Add `--systemd` to also install and enable the systemd user unit in one step:

```sh
curl -fsSL https://raw.githubusercontent.com/Toilal/air-drive/main/install.sh \
    | bash -s -- --systemd
```

### From source

```sh
cargo build --release            # → target/release/air-drive
install -m 0755 target/release/air-drive ~/.local/bin/
```

## Run as a systemd user service

```sh
air-drive setup --install-service
```

This copies
[`assets/systemd/air-drive.service`](../../assets/systemd/air-drive.service) to
`~/.config/systemd/user/air-drive.service` and runs
`systemctl --user enable --now air-drive.service`. Logs land in journald:

```sh
journalctl --user -u air-drive -f
```

To reverse it (the inverse operation, idempotent — it leaves your config,
state, tokens, account, mapping, and binary untouched):

```sh
air-drive setup --uninstall-service
```

## First run

You need to link a Google account and map a folder pair once.

### Guided

```sh
air-drive setup          # interactive: link → map → start --initial-sync
```

### Manual

```sh
air-drive link                          # OAuth desktop flow in your browser
air-drive map ~/Drive 'path:My Drive/Sync'
air-drive status                        # confirm account, mapping, counters
```

If the embedded OAuth client is unusable for you (e.g. `invalid_client`
errors), bootstrap your own Google Cloud client first — see
[OAuth setup](oauth-setup.md):

```sh
air-drive init --link
```

## File-manager integration (optional)

On GNOME (Ubuntu's default), air-drive can show a per-file sync-status emblem in
GNOME Files (Nautilus):

```sh
air-drive shell install   # installs the python3-nautilus bridge + the extension
killall nautilus          # fully restart the file manager to load it
```

This installs the `python3-nautilus` system package (it prompts for `sudo` on a
terminal, or prints the command to run) and deploys the extension under
`~/.local/share/nautilus-python/extensions/`. Emblems appear on synced files,
and on the sync folder itself when viewed from its parent (e.g. your Home
folder). Remove it with `air-drive shell uninstall`; check what's detected with
`air-drive shell status`.

> A plain `nautilus -q` can leave a cached background instance running (GNOME
> keeps Nautilus alive for the desktop), so prefer `killall nautilus` (or log
> out and back in) when emblems don't refresh. See the
> [CLI reference](cli.md#shell) for details.

## Next steps

- [CLI reference](cli.md) for every command and flag.
- [Configuration](configuration.md) to tune poll intervals, ignore patterns,
  and paths.
