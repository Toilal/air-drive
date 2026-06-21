//! Shared interactive helpers for CLI subcommands.
//!
//! The daemon runs in two distinct contexts:
//!
//! - **Interactive**: the user typed `air-drive map ...` (or another command)
//!   in a terminal. Stdin is a TTY; we can ask before taking a destructive or
//!   surprising action (e.g. creating a missing folder).
//! - **Non-interactive**: the daemon was started by systemd or piped from a
//!   script. Stdin is not a TTY; we must NOT block waiting for an answer the
//!   user cannot give. The conservative default is to refuse the action and
//!   surface an actionable error so the operator can fix the config and retry.
//!
//! [`confirm`] encapsulates that policy: TTY-only y/N prompt, default `No`,
//! returns `false` whenever the answer cannot be obtained. [`confirm_or_auto`]
//! is its mirror for actions that are the normal next step and safe to automate:
//! it proceeds automatically when non-interactive and only lets a TTY user veto.

use std::io::{BufRead, IsTerminal, Write};

use crate::error::Result;

/// Ask the user a yes/no question. Returns:
///
/// - `Ok(true)` when stdin is a TTY and the user typed `y` / `yes` (case
///   insensitive).
/// - `Ok(false)` when stdin is not a TTY, when EOF is reached, or when the
///   user typed anything else (including just pressing Enter — default No).
///
/// The prompt is written to stderr so it survives stdout redirection.
pub fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let mut stderr = std::io::stderr().lock();
    write!(stderr, "{question} [y/N]: ")?;
    stderr.flush()?;

    let stdin = std::io::stdin();
    let mut line = String::new();
    let n = stdin.lock().read_line(&mut line)?;
    if n == 0 {
        // EOF — caller can't answer.
        return Ok(false);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Ask a yes/no question that defaults to **proceeding** — the inverse policy of
/// [`confirm`]. Use it for an action that is the normal, safe next step but that
/// an interactive operator may still want to veto.
///
/// - `Ok(true)` when stdin is **not** a TTY (systemd, piped, CI): take the action
///   automatically, since there is no one to ask and it is the expected path.
/// - On a TTY: prompt `[Y/n]`, default Yes. Only an explicit `n` / `no` (case
///   insensitive) returns `Ok(false)`; empty input or EOF proceeds.
pub fn confirm_or_auto(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    let mut stderr = std::io::stderr().lock();
    write!(stderr, "{question} [Y/n]: ")?;
    stderr.flush()?;

    let stdin = std::io::stdin();
    let mut line = String::new();
    let n = stdin.lock().read_line(&mut line)?;
    if n == 0 {
        // EOF on a TTY — proceed with the default.
        return Ok(true);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(!(answer == "n" || answer == "no"))
}
