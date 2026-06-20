# OAuth setup

## Why air-drive needs the full `drive` scope

air-drive requests the broad `https://www.googleapis.com/auth/drive` scope on
the consent screen. Google's prompt wording is roughly *"See, edit, create, and
delete all of your Google Drive files"*.

The narrower `drive.file` scope only exposes files the daemon itself created,
which makes it impossible to sync an **already-populated** Drive folder — the
core job of a sync client. Per-folder write grants don't exist in Google's OAuth
surface, so a `drive.readonly` + `drive.file` combination would still hide
pre-existing content from the local watcher's perspective.

This is a deliberate, constitution-level decision; see
[`../../CLAUDE.md`](../../CLAUDE.md) §V for the full rationale.

### Trade-off: token lifetime in Testing mode

Google classifies `drive` as a **sensitive** scope. OAuth clients left in
`Testing` mode on the consent screen work fine but **cap refresh-token lifetime
at 7 days** — so the daemon will prompt for re-consent roughly once a week.

Moving a client to `Production` removes the cap but requires Google's OAuth
verification review (security assessment, homepage + privacy-policy URLs,
possibly a demo video).

## Using the embedded client

By default air-drive ships an embedded OAuth `client_id` (and its companion
`client_secret`, which is distributed with the app and not actually secret —
the Desktop flow is PKCE-based). For most users `air-drive link` just works:

```sh
air-drive link
```

## Using your own Google Cloud client

If the embedded client is unusable for you (for example `invalid_client`
errors, or you simply want your own project to own the consent), bootstrap a
personal client.

### Guided bootstrap

```sh
air-drive init --link
```

`init` walks you through creating a Google Cloud OAuth client of type
**Desktop** in the GCP Console, then writes the resulting `client_id` /
`client_secret` into `[oauth]` in your `config.toml`. With `--link` it runs
`air-drive link` immediately afterwards. Use `--force` to overwrite an existing
`[oauth].client_id`.

### Manual

If you prefer to do it by hand:

1. In the [Google Cloud Console](https://console.cloud.google.com/), create (or
   reuse) a project.
2. Enable the **Google Drive API** for that project.
3. Configure the **OAuth consent screen** (External), and add the
   `.../auth/drive` scope.
4. Create an **OAuth client ID** of type **Desktop app**.
5. Put the credentials into `config.toml`:

   ```toml
   [oauth]
   client_id = "...apps.googleusercontent.com"
   client_secret = "GOCSPX-..."
   ```

6. Run `air-drive link`.

See [configuration](configuration.md) for the full `[oauth]` reference.

## Where tokens live

The refresh token is persisted to `tokens.json` in the config directory
(`~/.config/air-drive` by default), in a directory created with mode `0700`
(owner-only). Removing the account with `air-drive unlink` deletes it.
