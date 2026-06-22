//! Pause/resume control plane.
//!
//! Two pieces work together:
//!
//! - [`PauseState`] — a `tokio::sync::watch` channel wrapped behind a small
//!   facade. The dispatcher reads `is_paused()` before pulling each op and
//!   `wait_for_resume()` to sleep cooperatively when paused. Clones share
//!   the same underlying flag.
//! - [`run_control_server`] — accepts connections on
//!   `<runtime_dir>/control.sock` and handles three commands: `pause`,
//!   `resume`, `status-snapshot`. Each command is one line; the response is
//!   one line (`ok\n`, `not running\n`, etc.). UNIX-only by design — the
//!   project is Linux-first per constitution principle V.
//!
//! Graceful shutdown is NOT a control-socket command: `air-drive stop` signals
//! the daemon directly (SIGTERM via the lock-file PID), reusing the same signal
//! path as Ctrl-C / `systemctl stop`.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};

/// File name of the control socket inside the runtime directory.
pub const CONTROL_SOCKET: &str = "control.sock";

/// Cloneable handle on the daemon's pause flag.
#[derive(Clone, Debug)]
pub struct PauseState {
    tx: watch::Sender<bool>,
    rx: watch::Receiver<bool>,
}

impl PauseState {
    /// Build a fresh PauseState, unpaused.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx, rx }
    }

    /// Are we currently paused?
    pub fn is_paused(&self) -> bool {
        *self.rx.borrow()
    }

    /// Flip into paused state. Idempotent.
    pub fn pause(&self) {
        let _ = self.tx.send(true);
    }

    /// Flip into running state. Idempotent.
    pub fn resume(&self) {
        let _ = self.tx.send(false);
    }

    /// Block until the flag becomes `false`. If already running, returns
    /// immediately. Used by the dispatcher loop to sleep cooperatively when
    /// paused instead of busy-looping.
    pub async fn wait_for_resume(&self) {
        let mut rx = self.rx.clone();
        while *rx.borrow() {
            if rx.changed().await.is_err() {
                return; // sender dropped
            }
        }
    }
}

impl Default for PauseState {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the absolute path the control socket lives at.
pub fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(CONTROL_SOCKET)
}

/// Spawn the control-socket listener. Returns when `cancel` fires. The socket
/// file is removed on shutdown so a fresh daemon doesn't trip over an `EADDRINUSE`
/// (the OS doesn't auto-unlink AF_UNIX paths).
pub async fn run_control_server(
    runtime_dir: PathBuf,
    state: PauseState,
    cancel: CancellationToken,
) -> Result<()> {
    std::fs::create_dir_all(&runtime_dir)?;
    let path = socket_path(&runtime_dir);
    // A stale socket from a previous crash makes `bind` fail with EADDRINUSE.
    // We hold the single-instance flock — if we get here, no other daemon is
    // listening, and any file we find is stale.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .map_err(|e| Error::Config(format!("bind control socket {}: {e}", path.display())))?;
    tracing::info!(socket = %path.display(), "control socket listening");

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            accept = listener.accept() => {
                let (stream, _) = match accept {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "control accept failed");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state).await {
                        tracing::warn!(error = %e, "control client failed");
                    }
                });
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Handle one control connection: one request line in, one response line out.
async fn handle_client(stream: UnixStream, state: PauseState) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let cmd = line.trim();

    let response: &str = match cmd {
        "pause" => {
            state.pause();
            tracing::info!("daemon paused via control socket");
            "ok\n"
        }
        "resume" => {
            state.resume();
            tracing::info!("daemon resumed via control socket");
            "ok\n"
        }
        "status-snapshot" => {
            // A richer snapshot reply (matching the JSON schema, with live
            // counters) lands when we wire the daemon's accumulated
            // `last_sync` + `last_error` state into the status path. For now,
            // `air-drive status` reads the DB directly.
            "{\"alive\":true}\n"
        }
        _ => "error: unknown command\n",
    };

    write.write_all(response.as_bytes()).await?;
    Ok(())
}

/// Client-side helper: connect to a daemon's control socket and send one
/// command. Returns the trimmed reply. Errors of kind `NotFound` /
/// `ConnectionRefused` translate to "no daemon running" at the caller.
pub async fn send_command(socket: &Path, command: &str) -> std::io::Result<String> {
    let stream = UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    write.write_all(command.as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    let mut reader = BufReader::new(read);
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    Ok(response.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn pause_state_round_trip() {
        let s = PauseState::new();
        assert!(!s.is_paused());
        s.pause();
        assert!(s.is_paused());
        s.resume();
        assert!(!s.is_paused());
    }

    #[tokio::test]
    async fn pause_state_is_clone_consistent() {
        let s = PauseState::new();
        let cloned = s.clone();
        s.pause();
        assert!(cloned.is_paused());
        cloned.resume();
        assert!(!s.is_paused());
    }

    #[tokio::test]
    async fn wait_for_resume_unblocks() {
        let s = PauseState::new();
        s.pause();
        let s2 = s.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            s2.resume();
        });
        // Should complete soon after the resume above.
        let res = tokio::time::timeout(Duration::from_millis(500), s.wait_for_resume()).await;
        assert!(res.is_ok(), "wait_for_resume never returned");
    }

    #[tokio::test]
    async fn control_server_round_trip_pause_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let state = PauseState::new();
        let cancel = CancellationToken::new();

        let server = tokio::spawn(run_control_server(
            tmp.path().to_path_buf(),
            state.clone(),
            cancel.clone(),
        ));
        // Give the server a beat to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sock = socket_path(tmp.path());

        let r = send_command(&sock, "pause").await.unwrap();
        assert_eq!(r, "ok");
        assert!(state.is_paused());

        let r = send_command(&sock, "resume").await.unwrap();
        assert_eq!(r, "ok");
        assert!(!state.is_paused());

        let r = send_command(&sock, "garbage").await.unwrap();
        assert!(r.starts_with("error"), "got {r}");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }
}
