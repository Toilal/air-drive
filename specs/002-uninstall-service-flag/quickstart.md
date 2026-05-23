# Quickstart: `air-drive setup --uninstall-service`

**Feature**: 002-uninstall-service-flag

This quickstart targets two audiences: end users who want to uninstall the daemon
service on their machine, and reviewers who want to validate the feature works as
expected before merging.

## End-user how-to

### Install the daemon as a systemd user unit (existing flow, for context)

```bash
air-drive setup --install-service
```

### Uninstall the daemon service (new in this feature)

```bash
air-drive setup --uninstall-service
```

What this does, in order:

1. Stops the running `air-drive.service` unit (if active) and disables auto-start on
   next login.
2. Removes `~/.config/systemd/user/air-drive.service`.
3. Refreshes the systemd user-scope cache so `systemctl --user list-unit-files` no
   longer reports the unit.

The command is idempotent: running it again on a clean system exits 0 with a message
indicating nothing was removed. It also exits 0 on a host without `systemctl`, in
which case it logs a warning and still removes the unit file if one is present.

### Full wipe (combine with `unlink`)

To remove the service AND the Drive account/tokens/mapping:

```bash
air-drive setup --uninstall-service
air-drive unlink
```

The watched local folder, the air-drive binary itself, and `config.toml` are left
alone. If you want a true clean slate:

```bash
air-drive setup --uninstall-service
air-drive unlink
rm -rf ~/.config/air-drive
rm ~/.local/bin/air-drive
```

## Validation recipe (reviewer / QA)

### Scenario 1 — Happy path (US1)

```bash
# Setup: install the unit so we have something to remove.
air-drive setup --install-service
systemctl --user is-active air-drive.service   # → active
test -f ~/.config/systemd/user/air-drive.service  # → exists

# Action.
air-drive setup --uninstall-service

# Verify.
systemctl --user is-active air-drive.service   # → inactive (or "Unit not loaded")
test -f ~/.config/systemd/user/air-drive.service && echo FAIL || echo OK
systemctl --user list-unit-files | grep air-drive || echo OK
```

Expected: all three checks output `OK` / show the unit absent. Exit code 0.

### Scenario 2 — Idempotency (US2)

```bash
# Starting from the clean state left by Scenario 1.
air-drive setup --uninstall-service && echo "no-op OK"
air-drive setup --uninstall-service && echo "still no-op OK"
```

Expected: both invocations exit 0, both print a message indicating nothing was removed.
Wall-clock time per invocation should be under 1 second (SC-003).

### Scenario 3 — Graceful degradation (US3)

```bash
# Make systemctl unavailable for this shell.
PATH_BACKUP="$PATH"
export PATH=/usr/bin:/bin
hash -r
# Drop a stray unit file in place.
mkdir -p ~/.config/systemd/user/
cp /path/to/repo/assets/systemd/air-drive.service ~/.config/systemd/user/

# Action (assumes `air-drive` is somewhere reachable, e.g. given by absolute path).
/path/to/air-drive setup --uninstall-service

# Verify.
test -f ~/.config/systemd/user/air-drive.service && echo FAIL || echo OK

# Restore.
export PATH="$PATH_BACKUP"
```

Expected: warning emitted about systemctl being unavailable, file removed, exit code 0.

### Scenario 4 — Mutually exclusive flags

```bash
air-drive setup --install-service --uninstall-service
echo "exit: $?"
```

Expected: clap-formatted error message ("the argument '--install-service' cannot be used
with '--uninstall-service'"), exit code non-zero (typically 2 from clap). No side
effects on disk or in systemd.

### Scenario 5 — Round-trip stress (SC-005)

```bash
for i in $(seq 1 10); do
  air-drive setup --install-service >/dev/null 2>&1
  air-drive setup --uninstall-service >/dev/null 2>&1
done

# Verify clean state.
test -f ~/.config/systemd/user/air-drive.service && echo FAIL || echo OK
systemctl --user is-enabled air-drive.service 2>&1 | grep -q "Failed to get unit" && echo OK
pgrep -f "air-drive start" && echo FAIL || echo OK
```

Expected: all three checks pass. No residual file, no residual enabled state, no
orphan processes.

## Manual rollback (if uninstall misbehaves before fix lands)

If the flag is broken and the user is stuck mid-uninstall, the manual sequence remains:

```bash
systemctl --user disable --now air-drive.service
rm ~/.config/systemd/user/air-drive.service
systemctl --user daemon-reload
```
