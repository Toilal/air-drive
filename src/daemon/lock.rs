//! Single-instance file lock for the daemon.
//!
//! The lock uses BSD-style `flock(2)` semantics via the [`nix`] crate. Locks are held
//! by the open file description, so two opens of `<config_dir>/lock` in the same
//! process compete for the lock (POSIX `fcntl` locks would not — this is intentional,
//! cf. the linked `nix::fcntl::flock` docs). The kernel releases the lock when the
//! `File` is closed (clean shutdown) or when the process dies.
//!
//! The PID is written into the file post-lock so a contender can name the running
//! daemon in its error message. The PID file is informational — the kernel-enforced
//! `flock` is the actual authority.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

use crate::error::{Error, Result};

/// File name of the lock file inside the config directory.
pub const LOCK_FILE: &str = "lock";

/// Guard returned by [`Lock::acquire`]. Dropping the guard releases the lock
/// (kernel-enforced when the `File` inside is closed; also on process death).
pub struct Lock {
    path: PathBuf,
    // `Flock<File>` owns the file and the lock together. Dropping it closes the file,
    // releasing the lock. The PID can no longer be rewritten after this struct exists,
    // but we don't need to — the PID was written at acquire time.
    _flock: Flock<File>,
}

impl Lock {
    /// Try to acquire the single-instance lock in `config_dir`.
    ///
    /// On contention with a live process, returns [`Error::Lock`] with the running
    /// daemon's PID (when readable). On a stale lock (PID file present but no live
    /// holder), the lock is reused.
    pub fn acquire(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join(LOCK_FILE);

        // Ensure the file exists so we always have an fd to flock.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        // Non-blocking exclusive lock. `Flock::lock` returns `Result<Flock<T>, (T, Errno)>`
        // — Ok takes ownership of the file together with the lock; Err returns the file
        // back so the caller can do something else with it. We drop the file on contention
        // (no further use for it) and surface a typed error carrying the holder's PID.
        let mut flock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(g) => g,
            Err((_file, _errno)) => {
                let pid = read_pid_from(&path);
                return Err(Error::Lock { pid });
            }
        };

        // We now hold the lock. Refresh the PID file. Truncating + seeking ensures we
        // don't leave a stale suffix when an old PID had more digits than the new one.
        write_current_pid(&mut flock)?;

        Ok(Self {
            path,
            _flock: flock,
        })
    }

    /// Path to the lock file (for diagnostics and tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lock")
            .field("path", &self.path)
            .field("fd", &self._flock.as_raw_fd())
            .finish()
    }
}

fn write_current_pid(flock: &mut Flock<File>) -> Result<()> {
    flock.set_len(0)?;
    flock.seek(SeekFrom::Start(0))?;
    write!(flock, "{}", std::process::id())?;
    flock.flush()?;
    Ok(())
}

/// Read the PID from the lock file. Returns `None` if the file is empty / unparseable.
fn read_pid_from(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_writes_current_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Lock::acquire(tmp.path()).expect("first acquire succeeds");
        let pid = std::fs::read_to_string(lock.path()).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
    }

    #[test]
    fn second_acquire_in_same_dir_fails_with_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let _first = Lock::acquire(tmp.path()).expect("first acquire");
        let err = Lock::acquire(tmp.path()).expect_err("second acquire must fail");
        match err {
            Error::Lock { pid } => {
                assert_eq!(pid, Some(std::process::id()));
            }
            other => panic!("expected Error::Lock, got {other:?}"),
        }
    }

    #[test]
    fn release_on_drop_lets_subsequent_acquire_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let _first = Lock::acquire(tmp.path()).unwrap();
        } // drop releases the kernel lock
        let _second = Lock::acquire(tmp.path()).expect("can re-acquire after release");
    }

    #[test]
    fn stale_pid_in_existing_file_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(LOCK_FILE);
        // Pre-populate the lock file with a bogus PID; flock is not held by anyone.
        std::fs::write(&lock_path, "999999999").unwrap();
        let lock = Lock::acquire(tmp.path()).expect("stale lock file should not block");
        let pid = std::fs::read_to_string(lock.path()).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
    }
}
