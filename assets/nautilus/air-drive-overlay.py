"""air-drive — Nautilus overlay extension.

Adds a per-file sync-status emblem in GNOME Files (Nautilus) by asking the
running air-drive daemon over its control socket, and keeps the emblems **live**
by subscribing to the daemon's activity stream. Deployed by
``air-drive shell install`` to
``~/.local/share/nautilus-python/extensions/air-drive-overlay.py`` and loaded by
the ``python3-nautilus`` bridge.

Design constraints:

- **Never break the file manager.** Every failure path (no daemon, socket
  missing, unknown command, timeout, malformed reply) degrades to "no emblem".
- **Cheap.** The per-file query is a single round-trip to a local UNIX socket
  with a short timeout; Nautilus calls ``update_file_info`` once per visible
  file.
- **Live.** A background thread holds a ``subscribe`` connection; when the
  daemon signals activity, the extension invalidates the files it has seen so
  Nautilus re-queries them and the emblems update without a manual refresh. All
  GTK calls are marshalled back to the main loop via ``GLib.idle_add``.

Protocol (line-based, see the daemon's control server):

- ``status-path <absolute-path>\\n`` → one status token: ``synced`` |
  ``syncing`` | ``pending`` | ``conflict`` | ``ignored`` | ``unknown``.
- ``subscribe\\n`` → a long-lived stream emitting ``changed\\n`` on each sync
  activity.
"""

import os
import socket
import threading
import time

import gi

# The Nautilus GIR namespace version tracks the installed nautilus-python
# (4.1 on GNOME 50, 4.0 on earlier GTK4 builds, 3.0 on legacy GTK3). Try the
# known versions in order so one script works across releases.
for _ver in ("4.1", "4.0", "3.0"):
    try:
        gi.require_version("Nautilus", _ver)
        break
    except ValueError:
        continue

from gi.repository import GLib, GObject, Nautilus  # noqa: E402  (after require_version)

# Map a daemon status token to a freedesktop emblem icon name. Tokens with no
# entry (``ignored``, ``unknown``) intentionally get no emblem.
_EMBLEM_FOR_STATUS = {
    "synced": "emblem-default",
    "syncing": "emblem-synchronizing",
    "pending": "emblem-synchronizing",
    "conflict": "emblem-important",
}

# Hard ceiling on the daemon round-trip. A local socket answers in well under a
# millisecond; this only bounds the pathological case so Nautilus never stalls.
_SOCKET_TIMEOUT_SECONDS = 0.2

# How long the subscriber waits before reconnecting after the stream drops or
# the daemon isn't running yet.
_RECONNECT_DELAY_SECONDS = 3.0


def _socket_path():
    """Locate the daemon control socket, or ``None`` if it isn't present.

    Mirrors the daemon's path resolution: ``$XDG_RUNTIME_DIR/air-drive`` first,
    then the ``<config-dir>/runtime`` fallback used when no runtime dir exists.
    """
    xdg_runtime = os.environ.get("XDG_RUNTIME_DIR")
    if xdg_runtime:
        candidate = os.path.join(xdg_runtime, "air-drive", "control.sock")
        if os.path.exists(candidate):
            return candidate
    fallback = os.path.join(
        os.path.expanduser("~"), ".config", "air-drive", "runtime", "control.sock"
    )
    if os.path.exists(fallback):
        return fallback
    return None


def _query_status(abs_path):
    """Return the daemon's status token for ``abs_path``, or ``None`` on any error."""
    sock_path = _socket_path()
    if sock_path is None:
        return None
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(_SOCKET_TIMEOUT_SECONDS)
            sock.connect(sock_path)
            sock.sendall(b"status-path " + abs_path.encode("utf-8") + b"\n")
            reply = sock.recv(256)
    except OSError:
        return None
    token = reply.decode("utf-8", "replace").strip()
    return token or None


class AirDriveOverlay(GObject.GObject, Nautilus.InfoProvider):
    """Sets a sync-status emblem on files tracked by air-drive, kept live."""

    def __init__(self):
        super().__init__()
        # path -> Nautilus.FileInfo we've emblemed, so we can invalidate them
        # when the daemon signals a change. Only touched on the main thread
        # (update_file_info and the idle callback both run there).
        self._seen = {}
        self._subscriber = threading.Thread(target=self._subscribe_loop, daemon=True)
        self._subscriber.start()

    def update_file_info(self, file):
        # Only local files have a meaningful sync status; skip trash://, sftp://,
        # recent:// and friends.
        if file.get_uri_scheme() != "file":
            return
        location = file.get_location()
        path = location.get_path() if location is not None else None
        if not path:
            return
        # Remember the file so a later activity signal can refresh its emblem.
        self._seen[path] = file
        status = _query_status(path)
        if status is None:
            return
        emblem = _EMBLEM_FOR_STATUS.get(status)
        if emblem is not None:
            file.add_emblem(emblem)

    def _subscribe_loop(self):
        """Background thread: hold a ``subscribe`` stream, retrying forever.

        On each ``changed`` line, ask the main loop to re-validate the files we
        have emblemed. Reconnects (with a delay) when the daemon is down or the
        connection drops, so emblems start updating once the daemon comes up.
        """
        while True:
            sock_path = _socket_path()
            if sock_path is None:
                time.sleep(_RECONNECT_DELAY_SECONDS)
                continue
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                    sock.connect(sock_path)
                    sock.sendall(b"subscribe\n")
                    buf = sock.makefile("r")
                    for line in buf:
                        if line.strip() == "changed":
                            GLib.idle_add(self._invalidate_seen)
            except OSError:
                pass
            time.sleep(_RECONNECT_DELAY_SECONDS)

    def _invalidate_seen(self):
        """Force Nautilus to re-query every file we've emblemed. Main thread."""
        for file in list(self._seen.values()):
            file.invalidate_extension_info()
        return GLib.SOURCE_REMOVE  # run once per idle scheduling
