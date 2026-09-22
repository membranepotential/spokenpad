//! The per-user daemon lock: one daemon per user, however it was started.
use crate::{
    core::control::Reply,
    shell::control::{Socket, refuse_until},
};
use anyhow::{Context, Result};
use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    sync::atomic::AtomicBool,
};

/// Another daemon of this user holds the lock. A daemon started by hand
/// exits with it, and `main` gives it exit code 3; one that systemd started
/// waits for the lock instead ([`lock_under_activation`]).
#[derive(Debug)]
pub struct AnotherDaemon(std::io::Error);

impl std::fmt::Display for AnotherDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another spokenpad daemon is running ({})", self.0)
    }
}

impl std::error::Error for AnotherDaemon {}

/// Holds the lock for the daemon lifetime. Never unlink a lock another process may hold.
pub(super) fn daemon_lock() -> Result<File> {
    let dir = crate::config::state_dir();
    crate::shell::dirs::create_private(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("daemon.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock borrows a valid owned file descriptor and retains no pointer.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(AnotherDaemon(std::io::Error::last_os_error()).into());
    }
    Ok(file)
}

/// The lock of a daemon that systemd started: waited for, not required.
///
/// Exiting without it would leave the presses queued on systemd's socket to
/// start this daemon again, and again, until the unit's start limit fails
/// `spokenpad.socket` and no key works any more. So it answers each press
/// with why it cannot serve ([`Reply::AnotherDaemon`] while a daemon started
/// by hand holds the lock, [`Reply::CannotLock`] while the state directory
/// cannot hold one), tries again before each press and every 100 ms, and
/// once it holds the lock goes on as if it had at once. `None` when told to
/// stop first.
pub(super) fn lock_under_activation(
    socket: &Socket,
    stopping: &AtomicBool,
) -> Result<Option<File>> {
    let mut logged: Option<String> = None;
    let lock = refuse_until(socket, stopping, || {
        daemon_lock().map_err(|e| {
            let reply = if e.is::<AnotherDaemon>() {
                Reply::AnotherDaemon
            } else {
                Reply::CannotLock
            };
            let reason = format!("{e:#}");
            if logged.as_ref() != Some(&reason) {
                log::error!(
                    "cannot take the daemon lock: {reason}; answering presses with \"{reply}\" until it can"
                );
                logged = Some(reason);
            }
            reply
        })
    })?;
    if lock.is_some() && logged.is_some() {
        log::info!("took the daemon lock");
    }
    Ok(lock)
}
